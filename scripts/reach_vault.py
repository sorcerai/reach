#!/usr/bin/env python3
"""Reach Vault: Out-of-Band Secret Broker for Reach + Hermes.

Stores credentials outside the microVM in ~/.reach/vault/secrets.json
(or an encrypted envelope) with strict filesystem permissions (0700/0600).
Supports:
  - domain mapping: maps domains (e.g. github.com, x.com) to username, password, totp_secret
  - RFC 6238 TOTP code generation (zero external dependencies)
  - in-memory credential injection via Reach MCP / synthetic input typing
    without ever writing passwords to container disk
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
    ) -> Dict[str, Any]:
        """Inject credentials directly into the active window on the target screen.

        Uses Reach MCP input typing (streamed synthetic X11 keypresses) or CDP
        without writing any password or secret to container disk.

        Enforces origin validation: verifies that the active tab's domain matches
        the target bound domain before typing any credentials.
        """
        canonical = normalize_domain(domain)
        creds = self.get(canonical)
        username = creds["username"]
        password = creds["password"]
        totp_secret = creds.get("totp_secret")

        api = (api_url or DEFAULT_REACH_API).rstrip("/")

        def _call_mcp(tool_name: str, arguments: Dict[str, Any]) -> Dict[str, Any]:
            if mcp_caller is not None:
                return mcp_caller(tool_name, arguments)

            payload = {
                "jsonrpc": "2.0",
                "id": int(time.time() * 1000) % 1_000_000,
                "method": "tools/call",
                "params": {"name": tool_name, "arguments": arguments},
            }
            req = urllib.request.Request(
                f"{api}/mcp",
                data=json.dumps(payload).encode("utf-8"),
                headers={"content-type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=30) as r:
                res = json.loads(r.read().decode("utf-8") or "{}")
                if "error" in res:
                    raise RuntimeError(f"MCP RPC Error: {res['error']}")
                return res.get("result", {})

        # Step 0: Read active tab URL and verify origin before typing secrets
        active_url = current_url
        if not active_url:
            try:
                page_info = _call_mcp("page_text", {"screen": screen})
                if isinstance(page_info, dict):
                    active_url = (
                        page_info.get("url")
                        or page_info.get("active_url")
                        or page_info.get("current_url")
                    )
                    if not active_url and "text" in page_info:
                        try:
                            parsed_text = json.loads(page_info["text"])
                            if isinstance(parsed_text, dict):
                                active_url = parsed_text.get("url")
                        except Exception:
                            pass
            except Exception as e:
                logger.debug("Failed to query page_text for active tab URL: %s", e)

        if not active_url:
            raise RuntimeError(
                f"Failed to inspect active tab URL before credential injection on screen {screen}"
            )

        validate_origin(active_url, canonical)

        # Step 1: Type username into currently focused field
        logger.info("Injecting username into screen %s for %s", screen, canonical)
        _call_mcp("type", {"text": username, "screen": screen})
        time.sleep(delay_sec)

        # Step 2: Tab into password field
        _call_mcp("key", {"combo": "Tab", "screen": screen})
        time.sleep(delay_sec)

        # Step 3: Type password (pure synthetic input; never written to file)
        logger.info("Injecting password into screen %s for %s (in-memory)", screen, canonical)
        _call_mcp("type", {"text": password, "screen": screen})
        time.sleep(delay_sec)

        # Step 4: Submit if requested
        if submit:
            _call_mcp("key", {"combo": "Return", "screen": screen})
            time.sleep(delay_sec)

        totp_code: Optional[str] = None
        if totp_secret:
            totp_code = generate_totp(totp_secret)
            if type_totp:
                # Give page a brief moment to transition to 2FA prompt
                time.sleep(max(1.0, delay_sec * 4))
                logger.info("Injecting 6-digit TOTP code into screen %s", screen)
                _call_mcp("type", {"text": totp_code, "screen": screen})
                if submit:
                    time.sleep(delay_sec)
                    _call_mcp("key", {"combo": "Return", "screen": screen})

        return {
            "status": "injected",
            "screen": screen,
            "domain": canonical,
            "username": username,
            "password_injected": True,
            "submitted": submit,
            "totp_generated": totp_code is not None,
            "totp_code": totp_code,
            "active_url": active_url,
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

    # get <domain> [--reveal]
    p_get = subparsers.add_parser("get", help="Retrieve credentials for a domain")
    p_get.add_argument("domain", help="Target domain")
    p_get.add_argument(
        "--reveal",
        action="store_true",
        help="Display raw plaintext password in output (default: redacted to prevent accidental exfiltration)",
    )

    # list
    subparsers.add_parser("list", help="List registered domains (redacting passwords)")

    # delete <domain>
    p_del = subparsers.add_parser("delete", help="Delete credentials for a domain")
    p_del.add_argument("domain", help="Target domain")

    # totp <domain> [--current-url <url>]
    p_totp = subparsers.add_parser("totp", help="Generate current 6-digit TOTP code")
    p_totp.add_argument("domain", help="Target domain")
    p_totp.add_argument(
        "--current-url",
        default=None,
        help="Active tab URL override for origin validation before revealing TOTP code",
    )

    # inject <screen> <domain>
    p_inj = subparsers.add_parser(
        "inject", help="Inject credentials into screen via input typing"
    )
    p_inj.add_argument("screen", type=int, help="Screen ID (e.g. 0)")
    p_inj.add_argument("domain", help="Target domain")
    p_inj.add_argument(
        "--api-url",
        default=DEFAULT_REACH_API,
        help="Reach API URL (default http://127.0.0.1:4200)",
    )
    p_inj.add_argument(
        "--no-submit",
        action="store_false",
        dest="submit",
        help="Do not press Return after typing password/TOTP",
    )
    p_inj.add_argument(
        "--no-totp",
        action="store_false",
        dest="type_totp",
        help="Do not type TOTP code if secret is present",
    )
    p_inj.add_argument(
        "--delay",
        type=float,
        default=0.25,
        help="Inter-keystroke/field delay in seconds (default 0.25)",
    )
    p_inj.add_argument(
        "--current-url",
        default=None,
        help="Active tab URL override for testing or manual origin verification",
    )

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
            if not getattr(args, "reveal", False):
                res["password"] = "[REDACTED]"
                res["_revealed"] = False
            else:
                res["_revealed"] = True
            print(json.dumps(res, indent=2))
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
            code = vault.get_totp(args.domain)
            print(json.dumps({"domain": args.domain, "totp": code}, indent=2))
            return 0

        if args.command == "inject":
            res = vault.inject(
                screen=args.screen,
                domain=args.domain,
                api_url=args.api_url,
                submit=args.submit,
                delay_sec=args.delay,
                type_totp=args.type_totp,
                current_url=args.current_url,
            )
            print(json.dumps(res, indent=2))
            return 0

    except Exception as e:
        sys.stderr.write(f"Error: {e}\n")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
