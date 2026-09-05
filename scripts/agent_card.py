#!/usr/bin/env python3
"""Reach Agent Card: Bounded Spending Engine & Checkout Injector.

Manages programmatically minted virtual cards for AI agents with bounded
spending limits, human-in-the-loop approval gates, and out-of-band synthetic
checkout form injection without exposing PAN/CVV to LLM context windows.

Storage:
  ~/.reach/cards/cards.json (mode 0600, dir 0700) or REACH_CARD_PATH.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass, field
import json
import logging
import os
from pathlib import Path
import re
import secrets
import stat
import sys
import tempfile
import time
from typing import Any, Callable, Dict, List, Optional, Union
import urllib.error
import urllib.parse
import urllib.request

logger = logging.getLogger("agent_card")

DEFAULT_CARDS_DIR = Path.home() / ".reach" / "cards"
DEFAULT_CARDS_FILE = DEFAULT_CARDS_DIR / "cards.json"
DEFAULT_APPROVAL_THRESHOLD_USD = 25.0
DEFAULT_REACH_API = os.environ.get("REACH_AGENT_URL", "http://127.0.0.1:4200")


# --------------------------------------------------------------------------
# Card Statuses
# --------------------------------------------------------------------------


class CardStatus:
    PENDING_APPROVAL = "PENDING_APPROVAL"
    ACTIVE = "ACTIVE"
    INJECTING = "INJECTING"
    CHARGED = "CHARGED"
    LOCKED = "LOCKED"
    EXPIRED = "EXPIRED"

    ALL = {PENDING_APPROVAL, ACTIVE, INJECTING, CHARGED, LOCKED, EXPIRED}


# --------------------------------------------------------------------------
# Domain Normalization & Helpers
# --------------------------------------------------------------------------


def normalize_domain(domain_or_url: str) -> str:
    """Normalize a domain or URL into a canonical lowercase hostname.

    Examples:
      - 'https://www.amazon.com/checkout' -> 'amazon.com'
      - 'store.google.com:443' -> 'store.google.com'
      - 'AMAZON.COM' -> 'amazon.com'
    """
    raw = domain_or_url.strip().lower()
    if not raw:
        return ""
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


def check_checkout_form_present(
    page_text: str = "",
    dom_html: str = "",
    form_info: Optional[Dict[str, Any]] = None,
) -> bool:
    """Check if a checkout-like form or credit card input is present on the page."""
    if form_info:
        if (
            form_info.get("has_form")
            or form_info.get("form_present")
            or form_info.get("has_checkout_form")
        ):
            return True

    combined = f"{page_text} {dom_html}".lower()

    # Check for card input field signatures in DOM / HTML
    cc_input_patterns = [
        r'autocomplete=["\'](?:cc-number|cc-csc|cc-exp)["\']',
        r'(?:name|id|placeholder)=["\'][^"\']*(?:card[-_]?num|cc[-_]?num|credit[-_]?card|cardnumber|cvv|cvc)[^"\']*["\']',
        r'type=["\'](?:tel|text|number)["\'][^>]+(?:card|cc|cvv)',
    ]
    for pat in cc_input_patterns:
        if re.search(pat, dom_html, re.IGNORECASE):
            return True

    # Check for checkout or credit card keywords in page text
    has_card_keyword = bool(
        re.search(
            r"\b(?:card\s*number|credit\s*card|debit\s*card|cardholder|cvv|cvc|security\s*code|exp(?:iration)?\s*date)\b",
            combined,
        )
    )
    has_checkout_keyword = bool(
        re.search(
            r"\b(?:checkout|payment|billing|place\s*order|pay\s*now|complete\s*purchase|order\s*summary)\b",
            combined,
        )
    )

    return has_card_keyword or has_checkout_keyword


def validate_luhn(card_number: str) -> bool:
    """Validate card number using standard Luhn algorithm."""
    digits = [int(c) for c in card_number if c.isdigit()]
    if len(digits) < 13:
        return False
    checksum = 0
    reverse_digits = digits[::-1]
    for i, d in enumerate(reverse_digits):
        if i % 2 == 1:
            d = d * 2
            if d > 9:
                d -= 9
        checksum += d
    return checksum % 10 == 0


def generate_synthetic_pan(prefix: str = "411122", length: int = 16) -> str:
    """Generate a synthetically valid PAN with correct Luhn checksum."""
    needed = length - 1 - len(prefix)
    random_digits = "".join(str(secrets.randbelow(10)) for _ in range(needed))
    partial = prefix + random_digits

    # Compute check digit so Luhn checksum is 0 mod 10
    digits = [int(c) for c in partial]
    checksum = 0
    # The check digit will be at index 0 when reversed, so partial digits start at index 1 reversed
    for i, d in enumerate(digits[::-1]):
        pos_from_right = i + 1
        if pos_from_right % 2 == 1:
            d = d * 2
            if d > 9:
                d -= 9
        checksum += d

    check_digit = (10 - (checksum % 10)) % 10
    pan = partial + str(check_digit)
    assert validate_luhn(pan)
    return pan


def mask_card_number(card_number: str) -> str:
    """Mask a 16-digit card number (e.g. '4111********4444')."""
    clean = re.sub(r"\D", "", card_number)
    if len(clean) <= 8:
        return "****"
    return f"{clean[:4]}{'*' * (len(clean) - 8)}{clean[-4:]}"


# --------------------------------------------------------------------------
# Card Schema
# --------------------------------------------------------------------------


@dataclass
class Card:
    """Virtual Card representation."""

    id: str
    card_number: str
    exp_month: str
    exp_year: str
    cvv: str
    merchant: str
    spending_limit_usd: float
    currency: str = "USD"
    status: str = CardStatus.ACTIVE
    created_at: int = field(default_factory=lambda: int(time.time()))
    idempotency_token: Optional[str] = None
    injected_at: Optional[float] = None

    def to_dict(self, mask: bool = False) -> Dict[str, Any]:
        data: Dict[str, Any] = {
            "id": self.id,
            "card_number": mask_card_number(self.card_number) if mask else self.card_number,
            "exp_month": self.exp_month,
            "exp_year": self.exp_year,
            "cvv": "***" if mask else self.cvv,
            "merchant": self.merchant,
            "spending_limit_usd": round(float(self.spending_limit_usd), 2),
            "currency": self.currency,
            "status": self.status,
            "created_at": self.created_at,
        }
        if self.idempotency_token is not None:
            data["idempotency_token"] = self.idempotency_token
        if self.injected_at is not None:
            data["injected_at"] = self.injected_at
        if mask:
            data["card_number_masked"] = mask_card_number(self.card_number)
        return data

    # Dictionary emulation for backwards/flexible compatibility
    def __getitem__(self, item: str) -> Any:
        return getattr(self, item)

    def __setitem__(self, key: str, value: Any) -> None:
        setattr(self, key, value)

    def get(self, key: str, default: Any = None) -> Any:
        return getattr(self, key, default)

    def keys(self):
        return self.to_dict().keys()

    def values(self):
        return self.to_dict().values()

    def items(self):
        return self.to_dict().items()

    def __contains__(self, key: str) -> bool:
        return hasattr(self, key)


# --------------------------------------------------------------------------
# Agent Card Bounded Spending Engine
# --------------------------------------------------------------------------


class AgentCardEngine:
    """Virtual Card Minting Engine and Vault Manager."""

    def __init__(self, cards_path: Optional[Union[str, Path]] = None) -> None:
        if cards_path is not None:
            self.cards_file = Path(cards_path)
        elif "REACH_CARD_PATH" in os.environ:
            self.cards_file = Path(os.environ["REACH_CARD_PATH"])
        else:
            self.cards_file = DEFAULT_CARDS_FILE
        self.cards_dir = self.cards_file.parent

    def _ensure_dir(self) -> None:
        """Create storage directory with strict 0700 permissions."""
        if not self.cards_dir.exists():
            self.cards_dir.mkdir(parents=True, mode=0o700, exist_ok=True)
        try:
            os.chmod(self.cards_dir, 0o700)
        except OSError:
            pass

    def _read_raw(self) -> Dict[str, Card]:
        """Read and parse card records from disk."""
        if not self.cards_file.exists():
            return {}

        with open(self.cards_file, "r", encoding="utf-8") as f:
            content = f.read().strip()
        if not content:
            return {}

        try:
            parsed = json.loads(content)
        except json.JSONDecodeError as e:
            raise ValueError(f"Failed to parse cards file JSON: {e}") from e

        cards_map: Dict[str, Card] = {}

        if isinstance(parsed, list):
            raw_list = parsed
        elif isinstance(parsed, dict):
            if "cards" in parsed and isinstance(parsed["cards"], list):
                raw_list = parsed["cards"]
            elif "cards" in parsed and isinstance(parsed["cards"], dict):
                raw_list = list(parsed["cards"].values())
            else:
                raw_list = list(parsed.values())
        else:
            raise ValueError("Cards file content must be a JSON object or array")

        for item in raw_list:
            if not isinstance(item, dict) or "id" not in item:
                continue
            raw_status = str(item.get("status", CardStatus.ACTIVE))
            # If a card was in INJECTING state when loaded from disk (e.g. from an
            # ungraceful crash mid-injection), treat it as LOCKED (burned / invalidated).
            if raw_status == CardStatus.INJECTING:
                raw_status = CardStatus.LOCKED
            card = Card(
                id=str(item["id"]),
                card_number=str(item.get("card_number", "")),
                exp_month=str(item.get("exp_month", "12")),
                exp_year=str(item.get("exp_year", "28")),
                cvv=str(item.get("cvv", "123")),
                merchant=str(item.get("merchant", "")),
                spending_limit_usd=float(item.get("spending_limit_usd", 0.0)),
                currency=str(item.get("currency", "USD")),
                status=raw_status,
                created_at=int(item.get("created_at", int(time.time()))),
                idempotency_token=item.get("idempotency_token"),
                injected_at=float(item["injected_at"]) if item.get("injected_at") is not None else None,
            )
            cards_map[card.id] = card

        return cards_map

    def _write_raw(self, cards_map: Dict[str, Card]) -> None:
        """Atomically persist cards to disk with 0600 permissions."""
        self._ensure_dir()
        serialized = {
            card_id: card.to_dict(mask=False)
            for card_id, card in cards_map.items()
        }
        out_str = json.dumps(serialized, indent=2)

        temp_file = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=str(self.cards_dir),
                prefix="cards_",
                suffix=".tmp",
                delete=False,
            ) as tf:
                temp_file = Path(tf.name)
                os.chmod(temp_file, 0o600)
                tf.write(out_str)
                tf.flush()
                os.fsync(tf.fileno())

            os.replace(temp_file, self.cards_file)
            try:
                os.chmod(self.cards_file, 0o600)
            except OSError:
                pass
        finally:
            if temp_file and temp_file.exists():
                try:
                    temp_file.unlink()
                except OSError:
                    pass

    def mint_card(
        self,
        merchant: str,
        spending_limit_usd: float,
        require_approval_threshold: float = DEFAULT_APPROVAL_THRESHOLD_USD,
        currency: str = "USD",
        card_id: Optional[str] = None,
        exp_month: Optional[str] = None,
        exp_year: Optional[str] = None,
        cvv: Optional[str] = None,
        pan: Optional[str] = None,
    ) -> Card:
        """Mint a new virtual card bounded to a merchant domain and spending limit.

        - If spending_limit_usd > require_approval_threshold: sets status to PENDING_APPROVAL.
        - If spending_limit_usd <= require_approval_threshold: automatically sets status to ACTIVE.
        """
        if spending_limit_usd < 0:
            raise ValueError("spending_limit_usd cannot be negative")

        canonical_merchant = normalize_domain(merchant)
        if not canonical_merchant:
            raise ValueError("merchant domain cannot be empty")

        cid = card_id or f"card_{secrets.token_hex(6)}"
        card_pan = pan or generate_synthetic_pan()
        month = exp_month or "12"
        year = exp_year or "28"
        card_cvv = cvv or f"{secrets.randbelow(900) + 100}"

        if spending_limit_usd > require_approval_threshold:
            status = CardStatus.PENDING_APPROVAL
        else:
            status = CardStatus.ACTIVE

        card = Card(
            id=cid,
            card_number=card_pan,
            exp_month=month,
            exp_year=year,
            cvv=card_cvv,
            merchant=canonical_merchant,
            spending_limit_usd=float(spending_limit_usd),
            currency=currency.upper(),
            status=status,
            created_at=int(time.time()),
        )

        cards = self._read_raw()
        cards[card.id] = card
        self._write_raw(cards)

        logger.info(
            "Minted card %s for merchant %s with limit $%.2f (status: %s)",
            card.id,
            card.merchant,
            card.spending_limit_usd,
            card.status,
        )
        return card

    def get_card(self, card_id: str) -> Card:
        """Retrieve a card by ID."""
        cards = self._read_raw()
        if card_id not in cards:
            raise KeyError(f"Card '{card_id}' not found")
        return cards[card_id]

    def approve_card(self, card_id: str) -> Card:
        """Human or supervisor approves spending on a pending card.

        Transitions status from PENDING_APPROVAL to ACTIVE.
        """
        cards = self._read_raw()
        if card_id not in cards:
            raise KeyError(f"Card '{card_id}' not found")

        card = cards[card_id]
        if card.status in {CardStatus.LOCKED, CardStatus.CHARGED, CardStatus.EXPIRED}:
            raise ValueError(f"Cannot approve card '{card_id}' in terminal status '{card.status}'")

        card.status = CardStatus.ACTIVE
        cards[card_id] = card
        self._write_raw(cards)

        logger.info("Approved card %s (now ACTIVE)", card_id)
        return card

    def lock_card(self, card_id: str) -> Card:
        """Explicitly lock a card so it cannot be used or recharged."""
        cards = self._read_raw()
        if card_id not in cards:
            raise KeyError(f"Card '{card_id}' not found")

        card = cards[card_id]
        card.status = CardStatus.LOCKED
        cards[card_id] = card
        self._write_raw(cards)

        logger.info("Locked card %s", card_id)
        return card

    def charge_card(
        self,
        card_id: str,
        amount_usd: float,
        merchant: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> Card:
        """Simulate charging a card.

        Enforces:
        - Card must be ACTIVE (not PENDING_APPROVAL, LOCKED, CHARGED, EXPIRED).
        - Amount cannot exceed spending_limit_usd.
        - Single-use: transitions status to CHARGED so it cannot be recharged.
        - Idempotency key: replaying with the same key returns the charged card.
        """
        if amount_usd < 0:
            raise ValueError("Charge amount cannot be negative")

        cards = self._read_raw()
        if card_id not in cards:
            raise KeyError(f"Card '{card_id}' not found")

        card = cards[card_id]

        if card.status == CardStatus.CHARGED:
            if idempotency_key and card.idempotency_token == idempotency_key:
                logger.info(
                    "Idempotent charge replay for card %s with key %s",
                    card_id,
                    idempotency_key,
                )
                return card

        if card.status != CardStatus.ACTIVE:
            raise ValueError(
                f"Cannot charge card '{card_id}' with status '{card.status}'. "
                "Card must be ACTIVE (approve if pending, or mint a new card if locked/charged)."
            )

        if amount_usd > card.spending_limit_usd:
            raise ValueError(
                f"Charge amount ${amount_usd:.2f} exceeds card spending limit of ${card.spending_limit_usd:.2f}"
            )

        if merchant:
            expected_merchant = normalize_domain(merchant)
            if expected_merchant != card.merchant:
                raise ValueError(
                    f"Merchant mismatch: card is bounded to '{card.merchant}', "
                    f"charge attempted by '{expected_merchant}'"
                )

        # Single-use: card transitions to CHARGED
        card.status = CardStatus.CHARGED
        if idempotency_key:
            card.idempotency_token = idempotency_key
        cards[card_id] = card
        self._write_raw(cards)

        logger.info("Charged card %s for $%.2f (now CHARGED)", card_id, amount_usd)
        return card

    def list_cards(
        self,
        merchant: Optional[str] = None,
        status: Optional[str] = None,
    ) -> List[Card]:
        """List virtual cards, optionally filtered by merchant or status."""
        cards = self._read_raw()
        results: List[Card] = []

        norm_merchant = normalize_domain(merchant) if merchant else None

        for card in cards.values():
            if norm_merchant and card.merchant != norm_merchant:
                continue
            if status and card.status != status:
                continue
            results.append(card)

        return results

    def delete_card(self, card_id: str) -> bool:
        """Delete a card record from disk."""
        cards = self._read_raw()
        if card_id in cards:
            del cards[card_id]
            self._write_raw(cards)
            return True
        return False

    def inject_card(
        self,
        screen: int,
        card_id: str,
        target_container: str = "agent-computer",
        api_url: Optional[str] = None,
        mcp_caller: Optional[Callable[[str, Dict[str, Any]], Dict[str, Any]]] = None,
        delay_sec: float = 0.25,
        submit: bool = False,
        split_exp: bool = False,
        method: str = "synthetic",
        current_url: Optional[str] = None,
        has_checkout_form: Optional[bool] = None,
        idempotency_token: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Inject card details directly into checkout form on the target screen.

        Specifications:
        - Card details (number, exp, cvv) are injected directly via CDP or synthetic input.
        - Card details NEVER appear in the LLM model prompt or conversation history.
        - Origin validation: verifies active page URL matches card merchant bound.
        - Requires checkout form or credit card input on the active page.
        - Idempotency check: prevents double-submitting a recently injected form.
        - Transition ACTIVE -> INJECTING (persisted) before first keystroke.
        - Transition INJECTING -> LOCKED immediately after typing.
        """
        cards = self._read_raw()
        if card_id not in cards:
            raise KeyError(f"Card '{card_id}' not found")
        card = cards[card_id]

        # Idempotency token check: if already injected with the same idempotency token, return cached result
        if idempotency_token and card.idempotency_token == idempotency_token:
            if card.status in {CardStatus.INJECTING, CardStatus.LOCKED}:
                logger.info(
                    "Idempotent injection replay for card %s with token %s; skipping double-submit",
                    card_id,
                    idempotency_token,
                )
                return {
                    "status": "already_injected",
                    "card_id": card_id,
                    "screen": screen,
                    "target_container": target_container,
                    "merchant": card.merchant,
                    "card_status": card.status,
                    "card_number_masked": mask_card_number(card.card_number),
                    "submitted": submit,
                    "method": method,
                    "idempotent_replay": True,
                    "idempotency_token": idempotency_token,
                }

        # Double-submit cooldown check
        now = time.time()
        if card.injected_at is not None and (now - card.injected_at) < 60:
            raise ValueError(
                f"Double-submit prevented: card '{card_id}' was injected recently at {card.injected_at}"
            )

        if card.status != CardStatus.ACTIVE:
            raise ValueError(
                f"Cannot inject card '{card_id}' with status '{card.status}'. "
                "Card must be ACTIVE."
            )

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
        page_text_res: Dict[str, Any] = {}
        if not active_url or has_checkout_form is None:
            try:
                page_text_res = _call_mcp("page_text", {"screen": screen})
                if not isinstance(page_text_res, dict):
                    page_text_res = {}
            except Exception as e:
                logger.debug("Failed to query page_text on screen %d: %s", screen, e)
                page_text_res = {}

            if not active_url:
                active_url = (
                    page_text_res.get("url")
                    or page_text_res.get("active_url")
                    or page_text_res.get("current_url")
                )
                if not active_url and "text" in page_text_res:
                    try:
                        parsed_text = json.loads(page_text_res["text"])
                        if isinstance(parsed_text, dict):
                            active_url = parsed_text.get("url")
                    except Exception:
                        pass

        if not active_url:
            raise RuntimeError("Failed to inspect active tab URL before card injection")

        validate_origin(active_url, card.merchant)

        # Verify checkout-like form or credit card input is present on the page
        if has_checkout_form is not None:
            form_present = bool(has_checkout_form)
        else:
            page_text = str(page_text_res.get("text", ""))
            dom_html = str(page_text_res.get("html", ""))
            form_present = check_checkout_form_present(
                page_text=page_text,
                dom_html=dom_html,
                form_info=page_text_res,
            )

        if not form_present:
            raise ValueError(
                f"Cannot inject card '{card_id}': no checkout form or credit card input detected on active page '{active_url}'"
            )

        # Transition ACTIVE -> INJECTING (persisted to disk) BEFORE the first keystroke is sent
        card.status = CardStatus.INJECTING
        card.injected_at = now
        effective_token = idempotency_token or f"tok_{secrets.token_hex(8)}"
        card.idempotency_token = effective_token
        cards[card_id] = card
        self._write_raw(cards)

        logger.info(
            "Injecting card %s (%s) into screen %d on container %s (origin verified: %s)",
            card.id,
            mask_card_number(card.card_number),
            screen,
            target_container,
            active_url,
        )

        # Step 0b: Re-verify active tab URL immediately before keystrokes/DOM injection (TOCTOU guard)
        if current_url is not None:
            try:
                live_check = _call_mcp("page_text", {"screen": screen})
                live_url = (
                    live_check.get("url")
                    or live_check.get("active_url")
                    or live_check.get("current_url")
                )
                if live_url:
                    validate_origin(live_url, card.merchant)
            except Exception as e:
                if isinstance(e, ValueError):
                    raise
                logger.debug("Pre-keystroke origin re-verification note: %s", e)

        if method == "cdp":
            # DOM field injection script
            cdp_script = f"""
            (() => {{
                function setVal(sel, val) {{
                    const el = document.querySelector(sel);
                    if (el) {{
                        el.value = val;
                        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                        return true;
                    }}
                    return false;
                }}
                const num = setVal('input[autocomplete="cc-number"], input[name*="card"], input[name*="cc_num"], input[id*="card_num"], input[id*="cardNumber"]', '{card.card_number}');
                const exp = setVal('input[autocomplete="cc-exp"], input[name*="exp"], input[id*="exp"]', '{card.exp_month}/{card.exp_year}');
                const cvv = setVal('input[autocomplete="cc-csc"], input[name*="cvv"], input[name*="cvc"], input[id*="cvv"]', '{card.cvv}');
                return {{ num, exp, cvv }};
            }})();
            """
            _call_mcp("playwright_eval", {"script": cdp_script, "screen": screen})
        else:
            # Synthetic keystrokes via Reach MCP:
            # Step 1: Type card number into active/focused field
            _call_mcp("type", {"text": card.card_number, "screen": screen})
            time.sleep(delay_sec)

            # Step 2: Tab to Expiration field
            _call_mcp("key", {"combo": "Tab", "screen": screen})
            time.sleep(delay_sec)

            # Step 3: Type Expiration date
            if split_exp:
                _call_mcp("type", {"text": card.exp_month, "screen": screen})
                time.sleep(delay_sec)
                _call_mcp("key", {"combo": "Tab", "screen": screen})
                time.sleep(delay_sec)
                _call_mcp("type", {"text": card.exp_year, "screen": screen})
            else:
                _call_mcp("type", {"text": f"{card.exp_month}/{card.exp_year}", "screen": screen})
            time.sleep(delay_sec)

            # Step 4: Tab to CVV field
            _call_mcp("key", {"combo": "Tab", "screen": screen})
            time.sleep(delay_sec)

            # Step 5: Type CVV
            _call_mcp("type", {"text": card.cvv, "screen": screen})
            time.sleep(delay_sec)

            # Step 6: Submit if requested
            if submit:
                _call_mcp("key", {"combo": "Return", "screen": screen})
                time.sleep(delay_sec)

        # Transition INJECTING -> LOCKED immediately after typing
        locked_card = self.lock_card(card_id)

        # Return masked payload: Card details NEVER appear in response or LLM prompt!
        return {
            "status": "injected",
            "card_id": card_id,
            "screen": screen,
            "target_container": target_container,
            "merchant": card.merchant,
            "card_status": locked_card.status,
            "card_number_masked": mask_card_number(card.card_number),
            "submitted": submit,
            "method": method,
            "active_url": active_url,
            "idempotency_token": effective_token,
        }


# --------------------------------------------------------------------------
# Top-level functional API
# --------------------------------------------------------------------------

_GLOBAL_ENGINE: Optional[AgentCardEngine] = None


def get_default_engine(cards_path: Optional[Union[str, Path]] = None) -> AgentCardEngine:
    global _GLOBAL_ENGINE
    if cards_path is not None:
        return AgentCardEngine(cards_path=cards_path)
    if _GLOBAL_ENGINE is None:
        _GLOBAL_ENGINE = AgentCardEngine()
    return _GLOBAL_ENGINE


def mint_card(
    merchant: str,
    spending_limit_usd: float,
    require_approval_threshold: float = DEFAULT_APPROVAL_THRESHOLD_USD,
    currency: str = "USD",
    cards_path: Optional[Union[str, Path]] = None,
    **kwargs: Any,
) -> Card:
    """Convenience functional API for minting virtual cards."""
    engine = get_default_engine(cards_path)
    return engine.mint_card(
        merchant=merchant,
        spending_limit_usd=spending_limit_usd,
        require_approval_threshold=require_approval_threshold,
        currency=currency,
        **kwargs,
    )


def approve_card(card_id: str, cards_path: Optional[Union[str, Path]] = None) -> Card:
    """Convenience functional API for approving virtual cards."""
    engine = get_default_engine(cards_path)
    return engine.approve_card(card_id=card_id)


def lock_card(card_id: str, cards_path: Optional[Union[str, Path]] = None) -> Card:
    """Convenience functional API for locking virtual cards."""
    engine = get_default_engine(cards_path)
    return engine.lock_card(card_id=card_id)


def charge_card(
    card_id: str,
    amount_usd: float,
    merchant: Optional[str] = None,
    cards_path: Optional[Union[str, Path]] = None,
    idempotency_key: Optional[str] = None,
) -> Card:
    """Convenience functional API for charging virtual cards."""
    engine = get_default_engine(cards_path)
    return engine.charge_card(
        card_id=card_id,
        amount_usd=amount_usd,
        merchant=merchant,
        idempotency_key=idempotency_key,
    )


def get_card(card_id: str, cards_path: Optional[Union[str, Path]] = None) -> Card:
    """Convenience functional API for getting a virtual card."""
    engine = get_default_engine(cards_path)
    return engine.get_card(card_id=card_id)


def list_cards(
    merchant: Optional[str] = None,
    status: Optional[str] = None,
    cards_path: Optional[Union[str, Path]] = None,
) -> List[Card]:
    """Convenience functional API for listing virtual cards."""
    engine = get_default_engine(cards_path)
    return engine.list_cards(merchant=merchant, status=status)


def inject_card(
    screen: int,
    card_id: str,
    target_container: str = "agent-computer",
    api_url: Optional[str] = None,
    mcp_caller: Optional[Callable[[str, Dict[str, Any]], Dict[str, Any]]] = None,
    delay_sec: float = 0.25,
    submit: bool = False,
    split_exp: bool = False,
    method: str = "synthetic",
    cards_path: Optional[Union[str, Path]] = None,
    current_url: Optional[str] = None,
    has_checkout_form: Optional[bool] = None,
    idempotency_token: Optional[str] = None,
) -> Dict[str, Any]:
    """Convenience functional API for out-of-band checkout injection."""
    engine = get_default_engine(cards_path)
    return engine.inject_card(
        screen=screen,
        card_id=card_id,
        target_container=target_container,
        api_url=api_url,
        mcp_caller=mcp_caller,
        delay_sec=delay_sec,
        submit=submit,
        split_exp=split_exp,
        method=method,
        current_url=current_url,
        has_checkout_form=has_checkout_form,
        idempotency_token=idempotency_token,
    )


# --------------------------------------------------------------------------
# CLI Entrypoint
# --------------------------------------------------------------------------


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="reach card",
        description="Agent Card Bounded Spending Engine & Checkout Injector",
    )
    parser.add_argument(
        "--cards-path",
        default=None,
        help="Path to cards.json (default ~/.reach/cards/cards.json)",
    )

    subparsers = parser.add_subparsers(dest="command", required=True)

    # mint --merchant <domain> --limit <amount> [--threshold <amount>]
    p_mint = subparsers.add_parser("mint", help="Mint a virtual card bounded to a merchant")
    p_mint.add_argument("--merchant", required=True, help="Target merchant domain (e.g. amazon.com)")
    p_mint.add_argument("--limit", type=float, required=True, help="Spending limit in USD")
    p_mint.add_argument(
        "--threshold",
        type=float,
        default=DEFAULT_APPROVAL_THRESHOLD_USD,
        help="Approval requirement threshold in USD (default 25.00)",
    )
    p_mint.add_argument("--currency", default="USD", help="Currency code (default USD)")

    # list
    p_list = subparsers.add_parser("list", help="List virtual cards")
    p_list.add_argument("--merchant", default=None, help="Filter by merchant domain")
    p_list.add_argument("--status", default=None, help="Filter by card status")
    p_list.add_argument(
        "--unmask",
        action="store_true",
        help="Include unmasked card numbers in output",
    )

    # approve <id>
    p_app = subparsers.add_parser("approve", help="Approve spending on a pending virtual card")
    p_app.add_argument("id", help="Card ID to approve")

    # lock <id>
    p_lock = subparsers.add_parser("lock", help="Lock a virtual card")
    p_lock.add_argument("id", help="Card ID to lock")

    # charge <id> --amount <amount>
    p_charge = subparsers.add_parser("charge", help="Simulate a charge against a card")
    p_charge.add_argument("id", help="Card ID to charge")
    p_charge.add_argument("--amount", type=float, required=True, help="Amount in USD")
    p_charge.add_argument("--merchant", default=None, help="Merchant domain attempting charge")
    p_charge.add_argument("--idempotency-key", default=None, help="Optional idempotency key for charge")

    # inject <id> [--screen <id>]
    p_inj = subparsers.add_parser("inject", help="Inject card details into checkout form")
    p_inj.add_argument("id", help="Card ID to inject")
    p_inj.add_argument("--screen", type=int, default=0, help="Screen ID (default 0)")
    p_inj.add_argument(
        "--container",
        default="agent-computer",
        help="Target container name (default agent-computer)",
    )
    p_inj.add_argument(
        "--api-url",
        default=DEFAULT_REACH_API,
        help="Reach API URL (default http://127.0.0.1:4200)",
    )
    p_inj.add_argument(
        "--submit",
        action="store_true",
        help="Press Return after typing CVV to submit form",
    )
    p_inj.add_argument(
        "--split-exp",
        action="store_true",
        help="Split expiration date into MM then Tab then YY",
    )
    p_inj.add_argument(
        "--delay",
        type=float,
        default=0.25,
        help="Inter-keystroke delay in seconds (default 0.25)",
    )
    p_inj.add_argument(
        "--current-url",
        default=None,
        help="Active tab URL override for testing or manual origin verification",
    )
    p_inj.add_argument(
        "--idempotency-token",
        default=None,
        help="Unique idempotency token to prevent duplicate submission",
    )

    args = parser.parse_args(argv)
    engine = AgentCardEngine(cards_path=args.cards_path)

    try:
        if args.command == "mint":
            card = engine.mint_card(
                merchant=args.merchant,
                spending_limit_usd=args.limit,
                require_approval_threshold=args.threshold,
                currency=args.currency,
            )
            print(json.dumps(card.to_dict(mask=True), indent=2))
            return 0

        if args.command == "list":
            cards = engine.list_cards(merchant=args.merchant, status=args.status)
            out = [c.to_dict(mask=not args.unmask) for c in cards]
            print(json.dumps(out, indent=2))
            return 0

        if args.command == "approve":
            card = engine.approve_card(args.id)
            print(json.dumps(card.to_dict(mask=True), indent=2))
            return 0

        if args.command == "lock":
            card = engine.lock_card(args.id)
            print(json.dumps(card.to_dict(mask=True), indent=2))
            return 0

        if args.command == "charge":
            card = engine.charge_card(
                card_id=args.id,
                amount_usd=args.amount,
                merchant=args.merchant,
                idempotency_key=args.idempotency_key,
            )
            print(json.dumps(card.to_dict(mask=True), indent=2))
            return 0

        if args.command == "inject":
            res = engine.inject_card(
                screen=args.screen,
                card_id=args.id,
                target_container=args.container,
                api_url=args.api_url,
                delay_sec=args.delay,
                submit=args.submit,
                split_exp=args.split_exp,
                current_url=args.current_url,
                idempotency_token=args.idempotency_token,
            )
            print(json.dumps(res, indent=2))
            return 0

    except Exception as e:
        sys.stderr.write(f"Error: {e}\n")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
