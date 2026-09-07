#!/usr/bin/env python3
"""Reach Vault: Out-of-Band Secret Broker for Reach + Hermes.

Stores credentials outside the microVM in ~/.reach/vault/secrets.json
(or an encrypted envelope) with strict filesystem permissions (0700/0600).
Supports:
  - domain mapping: maps domains (e.g. github.com, x.com) to username, password, totp_secret
  - RFC 6238 TOTP code generation (zero external dependencies)
  - authenticated host-native injection through the Reach MCP server, without
    exposing secret values to model context, scripts, or command arguments
  - optional encryption with PBKDF2-HMAC-SHA256 authenticated keystream
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import logging
import os
import re
import secrets
import struct
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, Optional, Tuple, Union

logger = logging.getLogger("reach_vault")
class _NoReachRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_API_OPENER = urllib.request.build_opener(_NoReachRedirect())

DEFAULT_VAULT_DIR = Path.home() / ".reach" / "vault"
DEFAULT_VAULT_FILE = DEFAULT_VAULT_DIR / "secrets.json"
DEFAULT_REACH_API = os.environ.get("REACH_AGENT_URL", "http://127.0.0.1:4200")


# --------------------------------------------------------------------------
# Domain Normalization
# --------------------------------------------------------------------------


def normalize_domain(domain_or_url: str) -> str:
    """Normalize a domain or URL into a clean canonical hostname.

    Examples:
      - 'https://github.com/login' -> 'github.com'
      - 'http://www.google.com:443/' -> 'google.com'
      - 'X.COM' -> 'x.com'
    """
    raw = domain_or_url.strip().lower()
    if "://" not in raw:
        raw = f"https://{raw}"
    parsed = urllib.parse.urlparse(raw)
    host = parsed.netloc or parsed.path.split("/")[0]
    if ":" in host:
        host = host.split(":")[0]
    if host.startswith("www."):
        host = host[4:]
    return host


def extract_etld_plus_one(domain_or_url: str) -> str:
    """Extract effective Top-Level Domain plus one label (eTLD+1).

    Handles common two-part public suffixes (e.g. .co.uk, .com.au, .co.jp, .org.uk)
    and single-part TLDs (e.g. .com, .org, .net, .io, .ai).
    """
    host = normalize_domain(domain_or_url)
    if not host:
        return ""
    if host in {"localhost", "127.0.0.1", "::1"} or host.endswith(".localhost"):
        return host

    parts = host.split(".")
    if len(parts) <= 2:
        return host

    known_multi_tenant = {
        "github.io", "pages.dev", "vercel.app", "herokuapp.com",
        "cloudfront.net", "web.app", "azurewebsites.net", "netlify.app", "s3.amazonaws.com"
    }
    for suffix in known_multi_tenant:
        if host.endswith("." + suffix):
            sub_part = host[: -(len(suffix) + 1)].split(".")[-1]
            return f"{sub_part}.{suffix}"

    known_second_levels = {"co", "com", "org", "net", "edu", "gov", "ac", "ne", "mil"}
    if len(parts) >= 3 and parts[-2] in known_second_levels and len(parts[-1]) == 2:
        return ".".join(parts[-3:])

    return ".".join(parts[-2:])


def validate_origin(active_url: str, bound_domain: str) -> None:
    """Validate active page URL against bound target domain.

    Requirements:
    1. Scheme must be 'https' (or 'http' only for localhost / 127.0.0.1).
    2. Normalized domain / eTLD+1 of active_url must match bound_domain.
    """
    if not active_url or not active_url.strip():
        raise ValueError("Cannot verify origin: active tab URL is empty or missing")

    raw = active_url.strip()
    parsed = urllib.parse.urlparse(raw if "://" in raw else f"https://{raw}")
    scheme = parsed.scheme.lower()
    host = (parsed.hostname or parsed.netloc or "").lower()
    if ":" in host:
        host = host.split(":")[0]

    is_local = (
        host in {"localhost", "127.0.0.1", "::1"}
        or host.endswith(".localhost")
        or host.startswith("127.")
    )

    if scheme == "http":
        if not is_local:
            raise ValueError(
                f"Insecure origin scheme 'http' for non-localhost URL '{active_url}'. Only https is allowed."
            )
    elif scheme != "https":
        raise ValueError(
            f"Invalid origin scheme '{scheme}' in URL '{active_url}'. Only https (or localhost http) is permitted."
        )

    active_etld = extract_etld_plus_one(host)
    bound_etld = extract_etld_plus_one(bound_domain)
    if not active_etld or not bound_etld or active_etld != bound_etld:
        raise ValueError(
            f"Origin mismatch: active URL '{active_url}' (eTLD+1: '{active_etld}') "
            f"does not match bound domain '{bound_domain}' (eTLD+1: '{bound_etld}')"
        )


# --------------------------------------------------------------------------
# RFC 6238 TOTP Generator (Pure Python standard library)
# --------------------------------------------------------------------------


def generate_totp(
    secret: str,
    for_time: Optional[float] = None,
    digits: int = 6,
    interval: int = 30,
) -> str:
    """Generate a standard RFC 6238 TOTP code from a base32 secret.

    Does not require external dependencies like pyotp.
    """
    cleaned_secret = re.sub(r"[\s\-]", "", secret).upper()
    # Add base32 padding if needed
    pad_len = (8 - len(cleaned_secret) % 8) % 8
    padded = cleaned_secret + ("=" * pad_len)
    try:
        key_bytes = base64.b32decode(padded)
    except Exception as e:
        raise ValueError(f"Invalid Base32 TOTP secret: {e}") from e

    target_time = time.time() if for_time is None else float(for_time)
    t = int(target_time) // interval
    msg = struct.pack(">Q", t)

    h = hmac.new(key_bytes, msg, hashlib.sha1).digest()
    offset = h[-1] & 0x0F
    truncated_hash = struct.unpack(">I", h[offset : offset + 4])[0] & 0x7FFFFFFF
    code = truncated_hash % (10**digits)
    return f"{code:0{digits}d}"


# --------------------------------------------------------------------------
# Encryption & Key Derivation (Zero external dependencies)
# --------------------------------------------------------------------------


def _derive_keys(passphrase: str, salt: bytes) -> Tuple[bytes, bytes]:
    """Derive 32-byte encryption key and 32-byte MAC key using PBKDF2."""
    derived = hashlib.pbkdf2_hmac(
        "sha256",
        passphrase.encode("utf-8"),
        salt,
        iterations=100_000,
        dklen=64,
    )
    return derived[:32], derived[32:]


def _xor_keystream(data: bytes, enc_key: bytes, nonce: bytes) -> bytes:
    """Generate keystream blocks via HMAC-SHA256 in counter mode and XOR with data."""
    output = bytearray(len(data))
    block_size = 32
    num_blocks = (len(data) + block_size - 1) // block_size
    for i in range(num_blocks):
        counter_bytes = struct.pack(">Q", i)
        block = hmac.new(enc_key, nonce + counter_bytes, hashlib.sha256).digest()
        start = i * block_size
        end = min(start + block_size, len(data))
        for j in range(start, end):
            output[j] = data[j] ^ block[j - start]
    return bytes(output)


def encrypt_data(plaintext: bytes, passphrase: str) -> Dict[str, Any]:
    """Encrypt byte payload with authenticated PBKDF2-HMAC-SHA256 keystream."""
    salt = secrets.token_bytes(16)
    nonce = secrets.token_bytes(16)
    enc_key, mac_key = _derive_keys(passphrase, salt)
    ciphertext = _xor_keystream(plaintext, enc_key, nonce)
    # Encrypt-then-MAC
    tag = hmac.new(mac_key, salt + nonce + ciphertext, hashlib.sha256).digest()
    return {
        "_version": 1,
        "_encrypted": True,
        "kdf": "pbkdf2_sha256",
        "iterations": 100_000,
        "salt": base64.b64encode(salt).decode("ascii"),
        "nonce": base64.b64encode(nonce).decode("ascii"),
        "ciphertext": base64.b64encode(ciphertext).decode("ascii"),
        "tag": base64.b64encode(tag).decode("ascii"),
    }


def decrypt_data(envelope: Dict[str, Any], passphrase: str) -> bytes:
    """Decrypt authenticated envelope created by encrypt_data."""
    if not envelope.get("_encrypted"):
        raise ValueError("Payload is not marked as encrypted")
    try:
        salt = base64.b64decode(envelope["salt"])
        nonce = base64.b64decode(envelope["nonce"])
        ciphertext = base64.b64decode(envelope["ciphertext"])
        expected_tag = base64.b64decode(envelope["tag"])
    except KeyError as e:
        raise ValueError(f"Malformed encrypted envelope: missing {e}") from e

    enc_key, mac_key = _derive_keys(passphrase, salt)
    computed_tag = hmac.new(
        mac_key, salt + nonce + ciphertext, hashlib.sha256
    ).digest()
    if not hmac.compare_digest(computed_tag, expected_tag):
        raise ValueError("Decryption failed: authentication tag mismatch or invalid key")

    return _xor_keystream(ciphertext, enc_key, nonce)


# --------------------------------------------------------------------------
# Reach Vault Manager
# --------------------------------------------------------------------------


class ReachVault:
    """Host-side credential vault storing secrets outside the microVM."""

    def __init__(
        self,
        vault_path: Optional[Union[str, Path]] = None,
        key: Optional[str] = None,
    ) -> None:
        if vault_path is not None:
            self.vault_file = Path(vault_path)
        elif "REACH_VAULT_PATH" in os.environ:
            self.vault_file = Path(os.environ["REACH_VAULT_PATH"])
        else:
            self.vault_file = DEFAULT_VAULT_FILE

        self.vault_dir = self.vault_file.parent
        self.key = key if key is not None else os.environ.get("REACH_VAULT_KEY")

    def _ensure_dir(self) -> None:
        """Create vault directory with 0700 permissions."""
        if not self.vault_dir.exists():
            self.vault_dir.mkdir(parents=True, mode=0o700, exist_ok=True)
        try:
            os.chmod(self.vault_dir, 0o700)
        except OSError:
            pass

    def _read_raw(self) -> Dict[str, Any]:
        """Read and parse vault data from disk, handling optional decryption."""
        if not self.vault_file.exists():
            return {}

        with open(self.vault_file, "r", encoding="utf-8") as f:
            content = f.read().strip()
        if not content:
            return {}

        try:
            parsed = json.loads(content)
        except json.JSONDecodeError as e:
            raise ValueError(f"Failed to parse vault file JSON: {e}") from e

        if isinstance(parsed, dict) and parsed.get("_encrypted"):
            if not self.key:
                raise ValueError(
                    f"Vault at {self.vault_file} is encrypted but no key was provided. "
                    "Set REACH_VAULT_KEY or pass --key."
                )
            decrypted_bytes = decrypt_data(parsed, self.key)
            return json.loads(decrypted_bytes.decode("utf-8"))

        if not isinstance(parsed, dict):
            raise ValueError("Vault content must be a JSON object mapping domains")
        return parsed

    def _write_raw(self, data: Dict[str, Any]) -> None:
        """Atomically write vault data to disk with 0600 permissions."""
        self._ensure_dir()
        serialized = json.dumps(data, indent=2)

        if self.key:
            payload = encrypt_data(serialized.encode("utf-8"), self.key)
            out_str = json.dumps(payload, indent=2)
        else:
            out_str = serialized

        # Atomic write via temporary file
        temp_file = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=str(self.vault_dir),
                prefix="vault_",
                suffix=".tmp",
                delete=False,
            ) as tf:
                temp_file = Path(tf.name)
                os.chmod(temp_file, 0o600)
                tf.write(out_str)
                tf.flush()
                os.fsync(tf.fileno())

            os.replace(temp_file, self.vault_file)
            try:
                os.chmod(self.vault_file, 0o600)
            except OSError:
                pass
        finally:
            if temp_file and temp_file.exists():
                try:
                    temp_file.unlink()
                except OSError:
                    pass

    def set(
        self,
        domain: str,
        username: str,
        password: str,
        totp_secret: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Store credentials for a domain."""
        canonical = normalize_domain(domain)
        entry: Dict[str, Any] = {
            "username": username,
            "password": password,
        }
        if totp_secret:
            # Validate totp secret format
            generate_totp(totp_secret, for_time=0)
            entry["totp_secret"] = totp_secret.strip()

        data = self._read_raw()
        data[canonical] = entry
        self._write_raw(data)
        logger.info("Saved credentials for domain %s in %s", canonical, self.vault_file)
        return {
            "domain": canonical,
            "username": username,
            "has_totp": bool(totp_secret),
        }

    def get(self, domain: str) -> Dict[str, Any]:
        """Retrieve credentials for a domain."""
        canonical = normalize_domain(domain)
        data = self._read_raw()
        if canonical not in data:
            raise KeyError(f"No credentials found for domain '{canonical}'")
        return dict(data[canonical])

    def delete(self, domain: str) -> bool:
        """Delete credentials for a domain."""
        canonical = normalize_domain(domain)
        data = self._read_raw()
        if canonical in data:
            del data[canonical]
            self._write_raw(data)
            return True
        return False

    def list_domains(self) -> Dict[str, Dict[str, Any]]:
        """List all domains registered in vault with metadata (redacting passwords)."""
        data = self._read_raw()
        results: Dict[str, Dict[str, Any]] = {}
        for domain, creds in data.items():
            results[domain] = {
                "username": creds.get("username", ""),
                "has_totp": bool(creds.get("totp_secret")),
            }
        return results

    def get_totp(self, domain: str, for_time: Optional[float] = None) -> str:
        """Generate the current 6-digit TOTP code for a domain."""
        creds = self.get(domain)
        secret = creds.get("totp_secret")
        if not secret:
            raise ValueError(f"No TOTP secret configured for domain '{domain}'")
        return generate_totp(secret, for_time=for_time)

    def inject(
        self,
        screen: int,
        domain: str,
        api_url: Optional[str] = None,
        submit: bool = True,
        delay_sec: float = 0.25,
        type_totp: bool = True,
        mcp_caller: Optional[Callable[[str, Dict[str, Any]], Dict[str, Any]]] = None,
        current_url: Optional[str] = None,
        lease_token: Optional[str] = None,
        handoff_gen: Optional[int] = None,
        observation_gen: Optional[int] = None,
    ) -> Dict[str, Any]:
        """Request host-native vault injection through the authenticated server.

        Secret values are never passed as tool arguments, generated scripts, or
        command-line arguments. The server resolves the native vault record.
        """
        canonical = normalize_domain(domain)
        if not canonical:
            raise ValueError("domain cannot be empty")
        request: Dict[str, Any] = {
            "kind": "vault",
            "domain": canonical,
            "submit": bool(submit),
        }
        if mcp_caller is not None:
            response = mcp_caller("inject", request)
        else:
            if not lease_token:
                raise ValueError("authenticated lease_token is required for injection")
            api = (api_url or DEFAULT_REACH_API).rstrip("/")
            payload = {
                "jsonrpc": "2.0",
                "id": int(time.time() * 1000) % 1_000_000,
                "method": "tools/call",
                "params": {"name": "inject", "arguments": {**request, "screen": screen}},
            }
            headers = {
                "content-type": "application/json",
                "X-Lease-Token": lease_token,
            }
            if handoff_gen is not None:
                headers["X-Handoff-Gen"] = str(handoff_gen)
            if observation_gen is not None:
                headers["X-Observation-Gen"] = str(observation_gen)
            req = urllib.request.Request(
                f"{api}/mcp", data=json.dumps(payload).encode("utf-8"),
                headers=headers, method="POST",
            )
            try:
                with _API_OPENER.open(req, timeout=30) as result:
                    response = json.loads(result.read().decode("utf-8") or "{}")
            except urllib.error.HTTPError as exc:
                try:
                    body = json.loads(exc.read().decode("utf-8", errors="replace"))
                except json.JSONDecodeError:
                    body = {}
                if exc.code == 428 and body.get("error") == "approval_required":
                    return {"status": "approval_required", "digest": body.get("digest")}
                if exc.code == 409:
                    return {"status": "stale_observation"}
                return {"status": "uncertain"}
            except (urllib.error.URLError, TimeoutError, OSError):
                return {"status": "uncertain"}

        if not isinstance(response, dict):
            return {"status": "uncertain"}
        if "result" in response:
            result = response.get("result")
            if not isinstance(result, dict) or result.get("isError") is not False:
                return {"status": "uncertain"}
            content = result.get("content")
            if isinstance(content, list):
                for part in content:
                    if isinstance(part, dict) and part.get("type") == "text":
                        try:
                            response = json.loads(part.get("text", ""))
                        except (TypeError, json.JSONDecodeError):
                            return {"status": "uncertain"}
                        break
        status = response.get("status")
        if status not in {"filled", "submitted", "auth_required", "rejected"}:
            if status == "uncertain":
                return {"status": "uncertain"}
            return {"status": "uncertain"}
        return {
            "status": status,
            "outcome": response.get("outcome", status),
            "submitted": bool(response.get("submitted", submit)),
            "domain": canonical,
        }


# --------------------------------------------------------------------------
# CLI Entry Point
# --------------------------------------------------------------------------


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="reach_vault",
        description="Reach Out-of-Band Secret Broker",
    )
    parser.add_argument(
        "--vault-path",
        default=None,
        help="Path to secrets.json (default ~/.reach/vault/secrets.json)",
    )
    parser.add_argument(
        "--key",
        default=None,
        help="Encryption passphrase (or set REACH_VAULT_KEY)",
    )

    subparsers = parser.add_subparsers(dest="command", required=True)

    # set <domain> --user <username> --pass <password> [--totp <secret>]
    p_set = subparsers.add_parser("set", help="Store credentials for a domain")
    p_set.add_argument("domain", help="Target domain (e.g. github.com)")
    p_set.add_argument("--user", required=True, help="Username or email")
    p_set.add_argument("--pass", dest="password", required=True, help="Password")
    p_set.add_argument("--totp", dest="totp", default=None, help="Optional TOTP base32 secret")

    # get <domain> (always metadata-only)
    p_get = subparsers.add_parser("get", help="Retrieve metadata for a domain")
    p_get.add_argument("domain", help="Target domain")

    # list
    subparsers.add_parser("list", help="List registered domains (redacting passwords)")

    # delete <domain>
    p_del = subparsers.add_parser("delete", help="Delete credentials for a domain")
    p_del.add_argument("domain", help="Target domain")
    # totp <domain> [--current-url <url>]
    p_totp = subparsers.add_parser("totp", help="Check whether a TOTP is configured")
    p_totp.add_argument("domain", help="Target domain")
    p_totp.add_argument("--current-url", default=None)
    # inject <screen> <domain>
    p_inj = subparsers.add_parser(
        "inject", help="Request authenticated host-native credential injection"
    )
    p_inj.add_argument("screen", type=int, help="Screen ID (e.g. 0)")
    p_inj.add_argument("domain", help="Target domain")
    p_inj.add_argument("--api-url", default=DEFAULT_REACH_API, help="Reach API URL")
    p_inj.add_argument("--no-submit", action="store_false", dest="submit")
    p_inj.add_argument("--lease-token", required=True, help="Authenticated lease capability")
    p_inj.add_argument("--handoff-gen", type=int, default=None)
    p_inj.add_argument("--observation-gen", type=int, default=None)

    args = parser.parse_args(argv)
    vault = ReachVault(vault_path=args.vault_path, key=args.key)

    try:
        if args.command == "set":
            res = vault.set(
                domain=args.domain,
                username=args.user,
                password=args.password,
                totp_secret=args.totp,
            )
            print(json.dumps(res, indent=2))
            return 0

        if args.command == "get":
            res = vault.get(args.domain)
            print(json.dumps({
                "domain": normalize_domain(args.domain),
                "username": bool(res.get("username")),
                "has_totp": bool(res.get("totp_secret")),
            }, indent=2))
            return 0

        if args.command == "list":
            res = vault.list_domains()
            print(json.dumps(res, indent=2))
            return 0

        if args.command == "delete":
            deleted = vault.delete(args.domain)
            print(json.dumps({"domain": args.domain, "deleted": deleted}, indent=2))
            return 0 if deleted else 1

        if args.command == "totp":
            if getattr(args, "current_url", None):
                validate_origin(args.current_url, args.domain)
            res = vault.list_domains().get(normalize_domain(args.domain), {})
            print(json.dumps({
                "domain": normalize_domain(args.domain),
                "has_totp": bool(res.get("has_totp")),
            }, indent=2))
            return 0

        if args.command == "inject":
            res = vault.inject(
                screen=args.screen,
                domain=args.domain,
                api_url=args.api_url,
                submit=args.submit,
                lease_token=args.lease_token,
                handoff_gen=args.handoff_gen,
                observation_gen=args.observation_gen,
            )
            print(json.dumps(res, indent=2))
            return 0 if res.get("status") not in {
                "uncertain", "approval_required", "stale_observation"
            } else 1

    except Exception as e:
        sys.stderr.write(f"Error: {e}\n")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
