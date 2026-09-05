"""Unit tests for Reach Out-of-Band Secret Broker (scripts/reach_vault.py)."""

import json
import os
import stat
import sys
import tempfile
from pathlib import Path
from unittest.mock import MagicMock

import pytest

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.reach_vault import (
    ReachVault,
    decrypt_data,
    encrypt_data,
    generate_totp,
    main,
    normalize_domain,
)


def test_domain_normalization() -> None:
    assert normalize_domain("https://github.com/login") == "github.com"
    assert normalize_domain("http://www.google.com:443/search?q=test") == "google.com"
    assert normalize_domain("X.COM") == "x.com"
    assert normalize_domain("sub.example.org/path") == "sub.example.org"
    assert normalize_domain("www.service.io") == "service.io"


def test_rfc6238_totp_test_vectors() -> None:
    # RFC 6238 Appendix B test vector:
    # Secret: ASCII "12345678901234567890" -> Base32 "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
    secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
    # At T=59 (t=1, interval=30): TOTP is 287082
    assert generate_totp(secret, for_time=59) == "287082"
    # At T=1111111109: TOTP is 081804
    assert generate_totp(secret, for_time=1111111109) == "081804"
    # At T=1234567890: TOTP is 005924
    assert generate_totp(secret, for_time=1234567890) == "005924"


def test_totp_with_spaces_and_lowercase() -> None:
    secret = "gezd gnbv gy3t qojq gezd gnbv gy3t qojq"
    assert generate_totp(secret, for_time=59) == "287082"


def test_totp_invalid_secret() -> None:
    with pytest.raises(ValueError, match="Invalid Base32"):
        generate_totp("INVALID_BASE32_198!@#")


def test_encryption_and_decryption() -> None:
    payload = b'{"secret_token": "super_sensitive_password_123"}'
    key = "correct-horse-battery-staple"

    encrypted = encrypt_data(payload, key)
    assert encrypted["_encrypted"] is True
    assert "salt" in encrypted
    assert "ciphertext" in encrypted
    assert "tag" in encrypted

    # Successful decryption
    decrypted = decrypt_data(encrypted, key)
    assert decrypted == payload

    # Decryption with wrong key fails
    with pytest.raises(ValueError, match="Decryption failed"):
        decrypt_data(encrypted, "wrong-key")


def test_vault_crud_and_permissions(tmp_path: Path) -> None:
    vault_file = tmp_path / "subdir" / "secrets.json"
    vault = ReachVault(vault_path=vault_file)

    # Initially empty
    assert vault.list_domains() == {}

    # Set github credentials
    res = vault.set(
        domain="https://github.com/login",
        username="dev_user",
        password="p@ssword_secret!",
        totp_secret="GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
    )
    assert res["domain"] == "github.com"
    assert res["username"] == "dev_user"
    assert res["has_totp"] is True

    # Verify file permissions
    vault_dir = vault_file.parent
    dir_mode = stat.S_IMODE(os.stat(vault_dir).st_mode)
    file_mode = stat.S_IMODE(os.stat(vault_file).st_mode)
    assert dir_mode == 0o700
    assert file_mode == 0o600

    # Get credentials
    creds = vault.get("github.com")
    assert creds["username"] == "dev_user"
    assert creds["password"] == "p@ssword_secret!"
    assert creds["totp_secret"] == "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"

    # Listing masks passwords
    listing = vault.list_domains()
    assert "github.com" in listing
    assert "password" not in listing["github.com"]
    assert listing["github.com"]["has_totp"] is True

    # TOTP generation from vault
    code = vault.get_totp("github.com", for_time=59)
    assert code == "287082"

    # Delete
    assert vault.delete("github.com") is True
    assert vault.delete("github.com") is False
    with pytest.raises(KeyError):
        vault.get("github.com")


def test_encrypted_vault_storage(tmp_path: Path) -> None:
    vault_file = tmp_path / "enc_vault.json"
    key = "master-passphrase-2026"

    # Write using encrypted vault
    vault = ReachVault(vault_path=vault_file, key=key)
    vault.set(domain="x.com", username="tester", password="secret_pass_456")

    # Read raw content directly from disk
    with open(vault_file, "r", encoding="utf-8") as f:
        raw_on_disk = json.loads(f.read())
    assert raw_on_disk.get("_encrypted") is True
    assert "secret_pass_456" not in json.dumps(raw_on_disk)

    # Reading without key should fail
    unauth_vault = ReachVault(vault_path=vault_file, key=None)
    with pytest.raises(ValueError, match="is encrypted but no key was provided"):
        unauth_vault.get("x.com")

    # Reading with wrong key should fail
    wrong_vault = ReachVault(vault_path=vault_file, key="wrong-pass")
    with pytest.raises(ValueError, match="Decryption failed"):
        wrong_vault.get("x.com")

    # Reading with correct key succeeds
    valid_vault = ReachVault(vault_path=vault_file, key=key)
    creds = valid_vault.get("x.com")
    assert creds["password"] == "secret_pass_456"


def test_inject_credentials_without_disk_write(tmp_path: Path) -> None:
    vault_file = tmp_path / "secrets.json"
    vault = ReachVault(vault_path=vault_file)
    vault.set(
        domain="github.com",
        username="octocat",
        password="super_top_secret_pass",
        totp_secret="GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
    )

    calls = []

    def mock_mcp_caller(tool_name: str, arguments: dict):
        calls.append((tool_name, arguments))
        if tool_name == "page_text":
            return {"url": "https://github.com/login", "status": "ok"}
        return {"status": "ok"}

    result = vault.inject(
        screen=1,
        domain="github.com",
        submit=True,
        delay_sec=0.01,
        type_totp=True,
        mcp_caller=mock_mcp_caller,
    )

    assert result["status"] == "injected"
    assert result["screen"] == 1
    assert result["domain"] == "github.com"
    assert result["active_url"] == "https://github.com/login"
    assert result["totp_generated"] is True
    assert result["totp_code"] is not None

    # Verify call sequence: page_text origin check happens BEFORE any keystroke is typed
    tool_names = [call[0] for call in calls]
    assert tool_names == ["page_text", "type", "key", "type", "key", "type", "key"]

    # Call 0: Origin check via page_text
    assert calls[0][1] == {"screen": 1}
    # Call 1: Type username
    assert calls[1][1] == {"text": "octocat", "screen": 1}
    # Call 2: Tab to password
    assert calls[2][1] == {"combo": "Tab", "screen": 1}
    # Call 3: Type password
    assert calls[3][1] == {"text": "super_top_secret_pass", "screen": 1}
    # Call 4: Enter to submit login
    assert calls[4][1] == {"combo": "Return", "screen": 1}
    # Call 5: Type 6-digit TOTP
    assert len(calls[5][1]["text"]) == 6
    assert calls[5][1]["screen"] == 1
    # Call 6: Enter to submit TOTP
    assert calls[6][1] == {"combo": "Return", "screen": 1}

    # Verify container disk was NOT touched:
    # no temporary files created containing the password
    for root, _, files in os.walk(tmp_path):
        for fname in files:
            p = os.path.join(root, fname)
            with open(p, "r", errors="ignore") as f:
                content = f.read()
                if fname != "secrets.json":
                    assert "super_top_secret_pass" not in content


def test_vault_inject_origin_validation(tmp_path: Path) -> None:
    vault_file = tmp_path / "secrets.json"
    vault = ReachVault(vault_path=vault_file)
    vault.set(
        domain="github.com",
        username="octocat",
        password="super_top_secret_pass",
    )

    # 1. Phishing domain rejected
    with pytest.raises(ValueError, match="Origin mismatch"):
        vault.inject(
            screen=0,
            domain="github.com",
            current_url="https://evil-github.com/login",
        )

    # 2. Insecure HTTP on public domain rejected
    with pytest.raises(ValueError, match="Insecure origin scheme"):
        vault.inject(
            screen=0,
            domain="github.com",
            current_url="http://github.com/login",
        )

    # 3. Missing active URL rejected
    with pytest.raises(RuntimeError, match="Failed to inspect active tab URL"):
        vault.inject(
            screen=0,
            domain="github.com",
            mcp_caller=lambda t, a: {"status": "ok"},  # No url returned
        )

    # 4. Valid subdomain matching eTLD+1 accepted
    calls = []
    res = vault.inject(
        screen=0,
        domain="github.com",
        current_url="https://auth.github.com/login",
        mcp_caller=lambda t, a: calls.append((t, a)) or {"status": "ok"},
    )
    assert res["status"] == "injected"
    assert res["active_url"] == "https://auth.github.com/login"

    # 5. Localhost HTTP accepted
    vault.set(domain="localhost", username="admin", password="devpassword")
    res_local = vault.inject(
        screen=0,
        domain="localhost",
        current_url="http://localhost:8000/login",
        mcp_caller=lambda t, a: {"status": "ok"},
    )
    assert res_local["status"] == "injected"


def test_cli_set_get_totp_list_delete(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    vault_file = str(tmp_path / "cli_vault.json")

    # CLI set
    ret = main(
        [
            "--vault-path",
            vault_file,
            "set",
            "example.com",
            "--user",
            "alice",
            "--pass",
            "alice123!",
            "--totp",
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
        ]
    )
    assert ret == 0
    captured = capsys.readouterr()
    assert "example.com" in captured.out

    # CLI get (default: redacted)
    ret = main(["--vault-path", vault_file, "get", "example.com"])
    assert ret == 0
    captured = capsys.readouterr()
    data = json.loads(captured.out)
    assert data["username"] == "alice"
    assert data["password"] == "[REDACTED]"
    assert data["_revealed"] is False

    # CLI get with --reveal
    ret = main(["--vault-path", vault_file, "get", "example.com", "--reveal"])
    assert ret == 0
    captured = capsys.readouterr()
    data_revealed = json.loads(captured.out)
    assert data_revealed["password"] == "alice123!"
    assert data_revealed["_revealed"] is True

    # CLI totp
    ret = main(["--vault-path", vault_file, "totp", "example.com"])
    assert ret == 0
    captured = capsys.readouterr()
    totp_json = json.loads(captured.out)
    assert len(totp_json["totp"]) == 6

    # CLI totp with valid origin
    ret = main(["--vault-path", vault_file, "totp", "example.com", "--current-url", "https://example.com/2fa"])
    assert ret == 0

    # CLI totp with mismatched origin fails with returncode 1
    ret = main(["--vault-path", vault_file, "totp", "example.com", "--current-url", "https://evil.com/2fa"])
    assert ret == 1
    captured = capsys.readouterr()
    assert "does not match bound domain" in captured.err

    # CLI list
    ret = main(["--vault-path", vault_file, "list"])
    assert ret == 0
    captured = capsys.readouterr()
    listed = json.loads(captured.out)
    assert "example.com" in listed
    assert "password" not in listed["example.com"]

    # CLI delete
    ret = main(["--vault-path", vault_file, "delete", "example.com"])
    assert ret == 0
    captured = capsys.readouterr()
    assert json.loads(captured.out)["deleted"] is True
