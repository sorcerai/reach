"""Unit tests for Reach Out-of-Band Secret Broker (scripts/reach_vault.py)."""

import json
import os
import stat
import sys
from pathlib import Path

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
    secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"  # RFC 6238 public test vector; gitleaks:allow
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
        totp_secret="GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",  # RFC 6238 public test vector; gitleaks:allow
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


def test_injection_sends_record_identifier_to_authenticated_server(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    vault_file = tmp_path / "secrets.json"
    vault = ReachVault(vault_path=vault_file)
    vault.set(
        domain="github.com",
        username="octocat",
        password="super_top_secret_pass",
        totp_secret="GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",  # RFC 6238 public test vector; gitleaks:allow
    )
    captured: dict[str, object] = {}

    class FakeResponse:
        def __enter__(self):
            return self

        def __exit__(self, exc_type, exc_value, traceback):
            return False

        def read(self) -> bytes:
            return json.dumps(
                {
                    "result": {
                        "isError": False,
                        "content": [
                            {
                                "type": "text",
                                "text": json.dumps(
                                    {
                                        "status": "filled",
                                        "outcome": "filled",
                                        "submitted": False,
                                    }
                                ),
                            }
                        ],
                    }
                }
            ).encode("utf-8")

    def open_request(request, timeout):
        captured["request"] = request
        return FakeResponse()

    monkeypatch.setattr("scripts.reach_vault._API_OPENER.open", open_request)
    result = vault.inject(
        screen=1,
        domain="github.com",
        submit=False,
        api_url="http://127.0.0.1:4200",
        lease_token="authenticated-lease",
    )

    request = captured["request"]
    body = json.loads(request.data.decode("utf-8"))
    arguments = body["params"]["arguments"]
    assert next(
        value for key, value in request.headers.items()
        if key.lower() == "x-lease-token"
    ) == "authenticated-lease"
    assert arguments == {
        "kind": "vault",
        "domain": "github.com",
        "submit": False,
        "screen": 1,
    }
    request_text = request.data.decode("utf-8")
    result_text = json.dumps(result)
    assert "super_top_secret_pass" not in request_text
    assert "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ" not in request_text
    assert "super_top_secret_pass" not in result_text
    assert "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ" not in result_text
    assert result == {
        "status": "filled",
        "outcome": "filled",
        "submitted": False,
        "domain": "github.com",
    }


def test_injection_requires_authenticated_lease(tmp_path: Path) -> None:
    vault = ReachVault(vault_path=tmp_path / "secrets.json")
    vault.set("github.com", "octocat", "super_top_secret_pass")

    with pytest.raises(ValueError, match="lease_token"):
        vault.inject(screen=0, domain="github.com")



def test_cli_secret_commands_return_metadata_only(
    tmp_path: Path, capsys: pytest.CaptureFixture
) -> None:
    vault_file = str(tmp_path / "cli_vault.json")
    password = "alice123!"
    totp_secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"  # RFC 6238 public test vector; gitleaks:allow

    ret = main(
        [
            "--vault-path",
            vault_file,
            "set",
            "example.com",
            "--user",
            "alice",
            "--pass",
            password,
            "--totp",
            totp_secret,
        ]
    )
    assert ret == 0
    captured = capsys.readouterr()
    assert password not in captured.out
    assert totp_secret not in captured.out

    ret = main(["--vault-path", vault_file, "get", "example.com"])
    assert ret == 0
    captured = capsys.readouterr()
    get_data = json.loads(captured.out)
    assert get_data == {
        "domain": "example.com",
        "username": True,
        "has_totp": True,
    }
    assert password not in captured.out
    assert totp_secret not in captured.out
    ret = main(["--vault-path", vault_file, "totp", "example.com"])
    assert ret == 0
    captured = capsys.readouterr()
    totp_data = json.loads(captured.out)
    assert totp_data == {"domain": "example.com", "has_totp": True}
    assert "totp_code" not in captured.out
    assert totp_secret not in captured.out

    ret = main(["--vault-path", vault_file, "list"])
    assert ret == 0
    captured = capsys.readouterr()
    listed = json.loads(captured.out)
    assert listed["example.com"] == {"username": "alice", "has_totp": True}
    assert password not in captured.out
    assert totp_secret not in captured.out

    ret = main(["--vault-path", vault_file, "delete", "example.com"])
    assert ret == 0
    assert json.loads(capsys.readouterr().out)["deleted"] is True
