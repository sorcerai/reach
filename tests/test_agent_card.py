"""Unit tests for Reach Agent Card Bounded Spending Engine & Checkout Injector."""

import json
import os
from pathlib import Path
import stat
import sys
from unittest.mock import MagicMock
import pytest

REPO_ROOT = Path(__file__).parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.agent_card import (
    AgentCardEngine,
    Card,
    CardStatus,
    approve_card,
    charge_card,
    generate_synthetic_pan,
    get_card,
    inject_card,
    list_cards,
    lock_card,
    main,
    mask_card_number,
    mint_card,
    normalize_domain,
    validate_luhn,
)


def test_domain_normalization() -> None:
    assert normalize_domain("https://amazon.com/dp/B000123") == "amazon.com"
    assert normalize_domain("http://www.google.com:443/checkout") == "google.com"
    assert normalize_domain("AMAZON.COM") == "amazon.com"
    assert normalize_domain("sub.shop.io/pay") == "sub.shop.io"
    assert normalize_domain("store.google.com") == "store.google.com"
    assert normalize_domain("") == ""


def test_luhn_validation_and_synthetic_pan() -> None:
    # Known test cards
    assert validate_luhn("4000000000000002") is True
    assert validate_luhn("4000000000000003") is False

    # Synthetic PAN generation
    for _ in range(10):
        pan = generate_synthetic_pan("411122")
        assert len(pan) == 16
        assert pan.startswith("411122")
        assert validate_luhn(pan) is True

    # Masking
    assert mask_card_number("4111222233334444") == "4111********4444"
    assert mask_card_number("12345678") == "****"


def test_mint_card_under_threshold_auto_active(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    # Spending limit <= $25.00 threshold -> status ACTIVE
    card = engine.mint_card(merchant="amazon.com", spending_limit_usd=20.00)
    assert card.status == CardStatus.ACTIVE
    assert card.merchant == "amazon.com"
    assert card.spending_limit_usd == 20.00
    assert card.currency == "USD"
    assert card.card_number.startswith("411122")
    assert validate_luhn(card.card_number) is True
    assert len(card.cvv) == 3

    # Boundary at exactly $25.00 -> ACTIVE
    card_boundary = engine.mint_card(merchant="amazon.com", spending_limit_usd=25.00)
    assert card_boundary.status == CardStatus.ACTIVE


def test_mint_card_above_threshold_pending_approval(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    # Spending limit $35.00 > $25.00 -> status PENDING_APPROVAL
    card = engine.mint_card(
        merchant="https://store.google.com/order",
        spending_limit_usd=35.00,
        require_approval_threshold=25.00,
    )
    assert card.status == CardStatus.PENDING_APPROVAL
    assert card.merchant == "store.google.com"
    assert card.spending_limit_usd == 35.00

    # Custom lower threshold ($10 limit with $5 threshold -> PENDING_APPROVAL)
    card_custom = engine.mint_card(
        merchant="shopify.com",
        spending_limit_usd=10.00,
        require_approval_threshold=5.00,
    )
    assert card_custom.status == CardStatus.PENDING_APPROVAL


def test_file_permissions_and_storage_integrity(tmp_path: Path) -> None:
    subdir = tmp_path / "custom_cards_dir"
    cards_file = subdir / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    card = engine.mint_card("amazon.com", 15.00)
    assert cards_file.exists()

    # Verify POSIX directory (0700) and file (0600) permissions
    dir_mode = stat.S_IMODE(os.stat(subdir).st_mode)
    file_mode = stat.S_IMODE(os.stat(cards_file).st_mode)
    assert dir_mode == 0o700
    assert file_mode == 0o600

    # Verify JSON content on disk
    with open(cards_file, "r", encoding="utf-8") as f:
        data = json.load(f)
    assert card.id in data
    assert data[card.id]["merchant"] == "amazon.com"
    assert data[card.id]["spending_limit_usd"] == 15.00


def test_approval_gate_enforcement(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    # Mint a card that requires approval ($50.00)
    card = engine.mint_card("amazon.com", 50.00, require_approval_threshold=25.00)
    assert card.status == CardStatus.PENDING_APPROVAL

    # 1. Attempting to charge pending card must fail
    with pytest.raises(ValueError, match="must be ACTIVE"):
        engine.charge_card(card.id, 20.00)

    # 2. Attempting to inject pending card must fail
    with pytest.raises(ValueError, match="must be ACTIVE"):
        engine.inject_card(screen=0, card_id=card.id)

    # 3. Approve the card -> status transitions to ACTIVE
    approved = engine.approve_card(card.id)
    assert approved.status == CardStatus.ACTIVE
    assert engine.get_card(card.id).status == CardStatus.ACTIVE

    # 4. Now charge succeeds
    charged = engine.charge_card(card.id, 45.00)
    assert charged.status == CardStatus.CHARGED

    # 5. Approving non-existent card raises KeyError
    with pytest.raises(KeyError):
        engine.approve_card("non_existent_card_id")

    # 6. Approving already charged or locked card raises ValueError
    with pytest.raises(ValueError, match="terminal status"):
        engine.approve_card(card.id)


def test_spending_limits(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    card = engine.mint_card("amazon.com", 35.00, require_approval_threshold=50.00)
    assert card.status == CardStatus.ACTIVE

    # Negative charge rejected
    with pytest.raises(ValueError, match="cannot be negative"):
        engine.charge_card(card.id, -10.00)

    # Exceeding limit ($36.00 > $35.00) rejected
    with pytest.raises(ValueError, match="exceeds card spending limit"):
        engine.charge_card(card.id, 36.00)

    # Merchant mismatch rejected
    with pytest.raises(ValueError, match="Merchant mismatch"):
        engine.charge_card(card.id, 20.00, merchant="walmart.com")

    # Valid charge within limit succeeds
    charged = engine.charge_card(card.id, 35.00, merchant="amazon.com")
    assert charged.status == CardStatus.CHARGED


def test_single_use_locking_after_charge(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    card = engine.mint_card("amazon.com", 20.00)
    assert card.status == CardStatus.ACTIVE

    # Charge card
    charged = engine.charge_card(card.id, 15.00)
    assert charged.status == CardStatus.CHARGED

    # Cannot recharge: card is locked after use
    with pytest.raises(ValueError, match="must be ACTIVE"):
        engine.charge_card(card.id, 5.00)

    # Cannot inject after charge
    with pytest.raises(ValueError, match="must be ACTIVE"):
        engine.inject_card(screen=0, card_id=card.id)


def test_lock_card(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    card = engine.mint_card("amazon.com", 20.00)
    assert card.status == CardStatus.ACTIVE

    locked = engine.lock_card(card.id)
    assert locked.status == CardStatus.LOCKED

    with pytest.raises(ValueError, match="must be ACTIVE"):
        engine.charge_card(card.id, 10.00)


def test_synthetic_form_injection_and_single_use_lock(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    card = engine.mint_card("amazon.com", 25.00)
    assert card.status == CardStatus.ACTIVE

    calls = []

    def mock_mcp(tool_name: str, arguments: dict):
        calls.append((tool_name, arguments))
        if tool_name == "page_text":
            return {
                "url": "https://amazon.com/checkout",
                "text": "Checkout: Enter Card Number, CVV, Expiration Date",
                "status": "ok",
            }
        return {"status": "ok"}

    res = engine.inject_card(
        screen=1,
        card_id=card.id,
        target_container="agent-computer",
        mcp_caller=mock_mcp,
        delay_sec=0.001,
        submit=True,
    )

    assert res["status"] == "injected"
    assert res["card_id"] == card.id
    assert res["screen"] == 1
    assert res["target_container"] == "agent-computer"
    assert res["card_status"] == CardStatus.LOCKED
    assert res["card_number_masked"] == card.to_dict(mask=True)["card_number_masked"]
    assert res["submitted"] is True

    # Card details NEVER appear in the returned dict (out-of-band security specification)
    assert "card_number" not in res
    assert "cvv" not in res
    assert card.card_number not in json.dumps(res)
    assert card.cvv not in json.dumps(res)

    # Verify input typing call sequence:
    # 0: page_text origin & form inspection
    # 1: type PAN
    # 2: key Tab
    # 3: type exp (MM/YY)
    # 4: key Tab
    # 5: type CVV
    # 6: key Return (submit)
    assert len(calls) == 7
    assert calls[0][0] == "page_text" and calls[0][1]["screen"] == 1
    assert calls[1][0] == "type" and calls[1][1]["text"] == card.card_number and calls[1][1]["screen"] == 1
    assert calls[2][0] == "key" and calls[2][1]["combo"] == "Tab"
    assert calls[3][0] == "type" and calls[3][1]["text"] == f"{card.exp_month}/{card.exp_year}"
    assert calls[4][0] == "key" and calls[4][1]["combo"] == "Tab"
    assert calls[5][0] == "type" and calls[5][1]["text"] == card.cvv
    assert calls[6][0] == "key" and calls[6][1]["combo"] == "Return"

    # Verify card status transitioned to LOCKED on disk
    persisted_card = engine.get_card(card.id)
    assert persisted_card.status == CardStatus.LOCKED

    # Second injection attempt must fail because card was just injected / is LOCKED
    with pytest.raises(ValueError, match=r"(must be ACTIVE|Double-submit prevented)"):
        engine.inject_card(screen=1, card_id=card.id, mcp_caller=mock_mcp)


def test_split_exp_and_cdp_injection(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    # Test split expiration (MM -> Tab -> YY)
    card1 = engine.mint_card("bestbuy.com", 20.00)
    calls1 = []

    def mock_mcp1(t: str, a: dict):
        calls1.append((t, a))
        if t == "page_text":
            return {
                "url": "https://bestbuy.com/checkout",
                "text": "Payment Checkout: Card Number CVV",
                "status": "ok",
            }
        return {"status": "ok"}

    engine.inject_card(
        screen=0,
        card_id=card1.id,
        mcp_caller=mock_mcp1,
        delay_sec=0.001,
        split_exp=True,
    )
    # calls: page_text -> type(PAN) -> Tab -> type(exp_month) -> Tab -> type(exp_year) -> Tab -> type(cvv)
    assert len(calls1) == 8
    assert calls1[0][0] == "page_text"
    assert calls1[1][0] == "type" and calls1[1][1]["text"] == card1.card_number
    assert calls1[3][1]["text"] == card1.exp_month
    assert calls1[4][1]["combo"] == "Tab"
    assert calls1[5][1]["text"] == card1.exp_year

    # Test CDP injection mode
    card2 = engine.mint_card("target.com", 20.00)
    calls2 = []

    def mock_mcp2(t: str, a: dict):
        calls2.append((t, a))
        if t == "page_text":
            return {
                "url": "https://target.com/checkout",
                "text": "Checkout: input[autocomplete=\"cc-number\"]",
                "status": "ok",
            }
        return {"status": "ok"}

    res2 = engine.inject_card(
        screen=0,
        card_id=card2.id,
        method="cdp",
        mcp_caller=mock_mcp2,
    )
    assert res2["status"] == "injected"
    assert res2["card_status"] == CardStatus.LOCKED
    assert len(calls2) == 2
    assert calls2[0][0] == "page_text"
    assert calls2[1][0] == "playwright_eval"
    assert "input[autocomplete=\"cc-number\"]" in calls2[1][1]["script"]


def test_list_and_filtering(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)

    c1 = engine.mint_card("amazon.com", 10.00)
    c2 = engine.mint_card("amazon.com", 50.00)
    c3 = engine.mint_card("google.com", 15.00)

    # All
    all_cards = engine.list_cards()
    assert len(all_cards) == 3

    # Filter by merchant
    amazon_cards = engine.list_cards(merchant="amazon.com")
    assert len(amazon_cards) == 2

    # Filter by status
    active_cards = engine.list_cards(status=CardStatus.ACTIVE)
    assert len(active_cards) == 2
    pending_cards = engine.list_cards(status=CardStatus.PENDING_APPROVAL)
    assert len(pending_cards) == 1
    assert pending_cards[0].id == c2.id


def test_top_level_functional_api(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"

    # Functional mint_card
    c = mint_card("amazon.com", 30.00, cards_path=cards_file)
    assert c.status == CardStatus.PENDING_APPROVAL

    # Functional get_card
    fetched = get_card(c.id, cards_path=cards_file)
    assert fetched.id == c.id

    # Functional approve_card
    app = approve_card(c.id, cards_path=cards_file)
    assert app.status == CardStatus.ACTIVE

    # Functional charge_card
    ch = charge_card(c.id, 25.00, cards_path=cards_file)
    assert ch.status == CardStatus.CHARGED

    # Functional lock_card on new card
    c2 = mint_card("ebay.com", 10.00, cards_path=cards_file)
    locked = lock_card(c2.id, cards_path=cards_file)
    assert locked.status == CardStatus.LOCKED

    # Functional list_cards
    listed = list_cards(cards_path=cards_file)
    assert len(listed) == 2


def test_cli_interface(tmp_path: Path, capsys: pytest.CaptureFixture) -> None:
    cards_file = str(tmp_path / "cli_cards.json")

    # CLI mint (under threshold -> ACTIVE)
    ret = main([
        "--cards-path", cards_file,
        "mint",
        "--merchant", "amazon.com",
        "--limit", "20.00",
    ])
    assert ret == 0
    out = json.loads(capsys.readouterr().out)
    assert out["status"] == CardStatus.ACTIVE
    card1_id = out["id"]
    assert out["spending_limit_usd"] == 20.00

    # CLI mint (above threshold -> PENDING_APPROVAL)
    ret = main([
        "--cards-path", cards_file,
        "mint",
        "--merchant", "apple.com",
        "--limit", "40.00",
    ])
    assert ret == 0
    out2 = json.loads(capsys.readouterr().out)
    assert out2["status"] == CardStatus.PENDING_APPROVAL
    card2_id = out2["id"]

    # CLI list
    ret = main(["--cards-path", cards_file, "list"])
    assert ret == 0
    listed = json.loads(capsys.readouterr().out)
    assert len(listed) == 2

    # CLI approve card2
    ret = main(["--cards-path", cards_file, "approve", card2_id])
    assert ret == 0
    app_out = json.loads(capsys.readouterr().out)
    assert app_out["status"] == CardStatus.ACTIVE

    # CLI charge card2
    ret = main([
        "--cards-path", cards_file,
        "charge", card2_id,
        "--amount", "30.00",
    ])
    assert ret == 0
    ch_out = json.loads(capsys.readouterr().out)
    assert ch_out["status"] == CardStatus.CHARGED

    # CLI lock card1
    ret = main(["--cards-path", cards_file, "lock", card1_id])
    assert ret == 0
    lock_out = json.loads(capsys.readouterr().out)
    assert lock_out["status"] == CardStatus.LOCKED


def test_card_inject_origin_validation(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)
    card = engine.mint_card("amazon.com", 25.00)

    # 1. Phishing mismatch rejection: active page is evil-phishing.com
    def mock_phishing(t, a):
        return {"url": "https://evil-phishing.com/checkout", "text": "Card Number", "status": "ok"}

    with pytest.raises(ValueError, match="Origin mismatch"):
        engine.inject_card(screen=0, card_id=card.id, mcp_caller=mock_phishing)

    # 2. Insecure HTTP scheme rejection for remote merchant
    def mock_http(t, a):
        return {"url": "http://amazon.com/checkout", "text": "Card Number", "status": "ok"}

    with pytest.raises(ValueError, match="Insecure origin"):
        engine.inject_card(screen=0, card_id=card.id, mcp_caller=mock_http)

    # 3. Missing active URL rejection
    def mock_no_url(t, a):
        return {"status": "ok"}

    with pytest.raises(RuntimeError, match="Failed to inspect active tab URL"):
        engine.inject_card(screen=0, card_id=card.id, mcp_caller=mock_no_url)

    # 4. Valid subdomain match on eTLD+1 succeeds
    calls = []
    def mock_subdomain(t, a):
        calls.append((t, a))
        return {"url": "https://pay.amazon.com/us/checkout", "text": "Card Number CVV", "status": "ok"}

    res = engine.inject_card(screen=0, card_id=card.id, mcp_caller=mock_subdomain)
    assert res["status"] == "injected"
    assert res["card_status"] == CardStatus.LOCKED

    # 5. Localhost over HTTP is allowed
    card_local = engine.mint_card("localhost", 10.00)
    calls_local = []
    def mock_local(t, a):
        calls_local.append((t, a))
        return {"url": "http://localhost:3000/pay", "text": "Checkout credit card", "status": "ok"}

    res_local = engine.inject_card(screen=0, card_id=card_local.id, mcp_caller=mock_local)
    assert res_local["status"] == "injected"


def test_card_inject_form_check(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)
    card = engine.mint_card("store.google.com", 25.00)

    # Page with no credit card / checkout form
    def mock_non_checkout(t, a):
        return {
            "url": "https://store.google.com/product/pixel",
            "text": "Product details: Google Pixel 8 Pro. 128GB Obsidian.",
            "status": "ok",
        }

    with pytest.raises(ValueError, match="no checkout form"):
        engine.inject_card(screen=0, card_id=card.id, mcp_caller=mock_non_checkout)

    # Card status must remain ACTIVE because injection was aborted before any typing
    assert engine.get_card(card.id).status == CardStatus.ACTIVE


def test_card_injecting_crash_recovery(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    # Simulate a crash where the card was left in INJECTING status on disk
    raw_card = {
        "card_crashed": {
            "id": "card_crashed",
            "card_number": "4111222233334444",
            "exp_month": "12",
            "exp_year": "28",
            "cvv": "123",
            "merchant": "amazon.com",
            "spending_limit_usd": 50.0,
            "currency": "USD",
            "status": "INJECTING",
            "created_at": 1700000000,
        }
    }
    cards_file.write_text(json.dumps(raw_card), encoding="utf-8")

    engine = AgentCardEngine(cards_path=cards_file)
    loaded_card = engine.get_card("card_crashed")
    # Must be safely treated as LOCKED (burned/invalidated)
    assert loaded_card.status == CardStatus.LOCKED


def test_card_inject_idempotency_and_cooldown(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)
    card = engine.mint_card("amazon.com", 25.00)

    calls = []
    def mock_mcp(t, a):
        calls.append((t, a))
        return {"url": "https://amazon.com/checkout", "text": "Card Number CVV", "status": "ok"}

    # 1. First injection with idempotency token
    res1 = engine.inject_card(
        screen=0,
        card_id=card.id,
        idempotency_token="idemp_tok_abc",
        mcp_caller=mock_mcp,
    )
    assert res1["status"] == "injected"
    assert res1["card_status"] == CardStatus.LOCKED
    calls_after_first = len(calls)

    # 2. Replay with same idempotency token -> idempotent replay without new keystrokes
    res2 = engine.inject_card(
        screen=0,
        card_id=card.id,
        idempotency_token="idemp_tok_abc",
        mcp_caller=mock_mcp,
    )
    assert res2["status"] == "already_injected"
    assert res2["idempotent_replay"] is True
    assert len(calls) == calls_after_first  # No additional calls

    # 3. Double-submit cooldown on a card injected without matching token
    card2 = engine.mint_card("amazon.com", 25.00)
    engine.inject_card(
        screen=0,
        card_id=card2.id,
        mcp_caller=mock_mcp,
    )
    # Re-inject immediately without token raises double-submit error
    with pytest.raises(ValueError, match="Double-submit prevented"):
        engine.inject_card(
            screen=0,
            card_id=card2.id,
            mcp_caller=mock_mcp,
        )


def test_charge_card_idempotency(tmp_path: Path) -> None:
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)
    card = engine.mint_card("amazon.com", 25.00)

    # Initial charge with idempotency key
    ch1 = engine.charge_card(card.id, 20.00, idempotency_key="key_123")
    assert ch1.status == CardStatus.CHARGED
    assert ch1.idempotency_token == "key_123"

    # Replaying same key returns the charged card idempotently
    ch2 = engine.charge_card(card.id, 20.00, idempotency_key="key_123")
    assert ch2.status == CardStatus.CHARGED
    assert ch2.idempotency_token == "key_123"

    # Attempting to charge with different key fails
    with pytest.raises(ValueError, match="must be ACTIVE"):
        engine.charge_card(card.id, 20.00, idempotency_key="key_different")


def test_card_inject_pre_keystroke_toctou_recheck(tmp_path: Path) -> None:
    """Verify that if a page navigates away or gets hijacked right before keystrokes, injection halts."""
    cards_file = tmp_path / "cards.json"
    engine = AgentCardEngine(cards_path=cards_file)
    card = engine.mint_card("amazon.com", 25.00)

    # Caller passed current_url="https://amazon.com/checkout", but live check reveals evil.com
    def mock_hijacked_mcp(tool_name: str, args: dict):
        if tool_name == "page_text":
            return {"url": "https://evil.com/phishing", "text": "Card Number", "status": "ok"}
        return {"status": "ok"}

    with pytest.raises(ValueError, match="Origin mismatch"):
        engine.inject_card(
            screen=0,
            card_id=card.id,
            current_url="https://amazon.com/checkout",
            has_checkout_form=True,
            mcp_caller=mock_hijacked_mcp,
        )

