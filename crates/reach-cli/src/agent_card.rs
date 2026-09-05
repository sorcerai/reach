//! Agent Card Bounded Spending Engine & Checkout Injector.
//!
//! Provides virtual card minting with merchant bounds and spending caps,
//! approval gate enforcement, single-use locking, and out-of-band checkout
//! injection into desktop sandboxes via synthetic inputs or Reach MCP.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Default approval threshold in USD.
pub const DEFAULT_APPROVAL_THRESHOLD_USD: f64 = 25.0;

/// Default virtual cards storage directory (~/.reach/cards).
pub fn default_cards_dir() -> PathBuf {
    dirs_or_home().join(".reach").join("cards")
}

/// Default virtual cards file (~/.reach/cards/cards.json).
pub fn default_cards_file() -> PathBuf {
    default_cards_dir().join("cards.json")
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Lifecycle status for a virtual card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CardStatus {
    PendingApproval,
    Active,
    Charged,
    Locked,
    Expired,
}

impl std::fmt::Display for CardStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CardStatus::PendingApproval => write!(f, "PENDING_APPROVAL"),
            CardStatus::Active => write!(f, "ACTIVE"),
            CardStatus::Charged => write!(f, "CHARGED"),
            CardStatus::Locked => write!(f, "LOCKED"),
            CardStatus::Expired => write!(f, "EXPIRED"),
        }
    }
}

/// Virtual Card data structure matching the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub card_number: String,
    pub exp_month: String,
    pub exp_year: String,
    pub cvv: String,
    pub merchant: String,
    pub spending_limit_usd: f64,
    pub currency: String,
    pub status: CardStatus,
    pub created_at: i64,
}

impl Card {
    /// Mask the card number (e.g. "4111********4444").
    pub fn masked_card_number(&self) -> String {
        mask_card_number(&self.card_number)
    }

    /// Redacted view for safe logging/display.
    pub fn to_safe_view(&self) -> SafeCardView {
        SafeCardView {
            id: self.id.clone(),
            card_number_masked: self.masked_card_number(),
            exp_month: self.exp_month.clone(),
            exp_year: self.exp_year.clone(),
            cvv: "***".into(),
            merchant: self.merchant.clone(),
            spending_limit_usd: (self.spending_limit_usd * 100.0).round() / 100.0,
            currency: self.currency.clone(),
            status: self.status,
            created_at: self.created_at,
        }
    }
}

/// Safe view of a card with masked PAN and redacted CVV.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafeCardView {
    pub id: String,
    pub card_number_masked: String,
    pub exp_month: String,
    pub exp_year: String,
    pub cvv: String,
    pub merchant: String,
    pub spending_limit_usd: f64,
    pub currency: String,
    pub status: CardStatus,
    pub created_at: i64,
}

/// Normalize merchant domain string or URL.
pub fn normalize_domain(domain_or_url: &str) -> String {
    let raw = domain_or_url.trim().to_lowercase();
    if raw.is_empty() {
        return String::new();
    }
    let host_part = raw
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    let mut h = host_part.to_string();
    if h.starts_with("www.") {
        h = h[4..].to_string();
    }
    h
}

/// Check card number with Luhn algorithm.
pub fn validate_luhn(card_number: &str) -> bool {
    let digits: Vec<u32> = card_number.chars().filter_map(|c| c.to_digit(10)).collect();
    if digits.len() < 13 {
        return false;
    }
    let mut checksum = 0;
    for (i, &d) in digits.iter().rev().enumerate() {
        let mut val = d;
        if i % 2 == 1 {
            val *= 2;
            if val > 9 {
                val -= 9;
            }
        }
        checksum += val;
    }
    checksum % 10 == 0
}

/// Generate a synthetic 16-digit PAN starting with prefix and valid Luhn check digit.
pub fn generate_synthetic_pan(prefix: &str) -> String {
    let target_len = 16;
    let needed = target_len - 1 - prefix.len();
    let mut rng_digits = String::with_capacity(needed);
    for _ in 0..needed {
        let n: u8 = (std::process::id() as u64
            ^ SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64) as u8
            % 10;
        rng_digits.push_str(&n.to_string());
    }

    // Use uuid for extra randomness if needed
    let uuid_digits: String = uuid::Uuid::new_v4()
        .as_simple()
        .to_string()
        .chars()
        .filter_map(|c| c.to_digit(10))
        .take(needed)
        .map(|d| char::from_digit(d, 10).unwrap())
        .collect();

    let random_part = if uuid_digits.len() == needed {
        uuid_digits
    } else {
        format!("{rng_digits:0<needed$}")
    };

    let partial = format!("{prefix}{random_part}");
    let digits: Vec<u32> = partial.chars().filter_map(|c| c.to_digit(10)).collect();

    let mut checksum = 0;
    for (i, &d) in digits.iter().rev().enumerate() {
        let pos_from_right = i + 1;
        let mut val = d;
        if pos_from_right % 2 == 1 {
            val *= 2;
            if val > 9 {
                val -= 9;
            }
        }
        checksum += val;
    }

    let check_digit = (10 - (checksum % 10)) % 10;
    let pan = format!("{partial}{check_digit}");
    pan
}

/// Mask a card number showing only the first 4 and last 4 digits.
pub fn mask_card_number(pan: &str) -> String {
    let clean: String = pan.chars().filter(|c| c.is_ascii_digit()).collect();
    if clean.len() <= 8 {
        return "****".to_string();
    }
    format!(
        "{}{}{}",
        &clean[..4],
        "*".repeat(clean.len() - 8),
        &clean[clean.len() - 4..]
    )
}

/// Result of an out-of-band checkout injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardInjectionResult {
    pub status: String,
    pub card_id: String,
    pub screen: u32,
    pub target_container: String,
    pub merchant: String,
    pub card_status: CardStatus,
    pub card_number_masked: String,
    pub submitted: bool,
    pub commands: Vec<String>,
}

/// Agent Card Storage and Engine.
pub struct AgentCardEngine {
    pub cards_file: PathBuf,
    pub cards_dir: PathBuf,
}

impl AgentCardEngine {
    /// Instantiate engine using explicit path or environment variable / defaults.
    pub fn new(custom_path: Option<PathBuf>) -> Self {
        let cards_file = custom_path
            .or_else(|| std::env::var("REACH_CARD_PATH").ok().map(PathBuf::from))
            .unwrap_or_else(default_cards_file);
        let cards_dir = cards_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_cards_dir);

        Self {
            cards_file,
            cards_dir,
        }
    }

    fn ensure_dir(&self) -> Result<()> {
        if !self.cards_dir.exists() {
            fs::create_dir_all(&self.cards_dir)
                .with_context(|| format!("Failed to create directory {:?}", self.cards_dir))?;
        }
        #[cfg(unix)]
        {
            let perm = fs::Permissions::from_mode(0o700);
            let _ = fs::set_permissions(&self.cards_dir, perm);
        }
        Ok(())
    }

    fn read_raw(&self) -> Result<HashMap<String, Card>> {
        if !self.cards_file.exists() {
            return Ok(HashMap::new());
        }

        let content = fs::read_to_string(&self.cards_file)
            .with_context(|| format!("Failed to read cards file {:?}", self.cards_file))?;
        if content.trim().is_empty() {
            return Ok(HashMap::new());
        }

        // Parse either a map or list
        if let Ok(map) = serde_json::from_str::<HashMap<String, Card>>(&content) {
            return Ok(map);
        }

        if let Ok(list) = serde_json::from_str::<Vec<Card>>(&content) {
            let mut map = HashMap::new();
            for c in list {
                map.insert(c.id.clone(), c);
            }
            return Ok(map);
        }

        bail!("Failed to deserialize cards.json into card map or list");
    }

    fn write_raw(&self, cards: &HashMap<String, Card>) -> Result<()> {
        self.ensure_dir()?;
        let json_str = serde_json::to_string_pretty(cards)?;

        let temp_file = self
            .cards_dir
            .join(format!(".cards_{}.tmp", uuid::Uuid::new_v4()));

        fs::write(&temp_file, &json_str)
            .with_context(|| format!("Failed to write temporary cards file {:?}", temp_file))?;

        #[cfg(unix)]
        {
            let perm = fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(&temp_file, perm);
        }

        fs::rename(&temp_file, &self.cards_file).with_context(|| {
            format!("Failed to rename {:?} to {:?}", temp_file, self.cards_file)
        })?;

        #[cfg(unix)]
        {
            let perm = fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(&self.cards_file, perm);
        }

        Ok(())
    }

    /// Mint a new virtual card.
    ///
    /// - If spending_limit_usd > require_approval_threshold: sets status to PENDING_APPROVAL.
    /// - If spending_limit_usd <= require_approval_threshold: automatically sets status to ACTIVE.
    pub fn mint_card(
        &mut self,
        merchant: &str,
        spending_limit_usd: f64,
        require_approval_threshold: f64,
        currency: Option<&str>,
        custom_id: Option<&str>,
    ) -> Result<Card> {
        if spending_limit_usd < 0.0 {
            bail!("spending_limit_usd cannot be negative");
        }

        let canonical_merchant = normalize_domain(merchant);
        if canonical_merchant.is_empty() {
            bail!("merchant domain cannot be empty");
        }

        let cid = custom_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("card_{}", &uuid::Uuid::new_v4().to_string()[..8]));

        let pan = generate_synthetic_pan("411122");
        let status = if spending_limit_usd > require_approval_threshold {
            CardStatus::PendingApproval
        } else {
            CardStatus::Active
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let card = Card {
            id: cid,
            card_number: pan,
            exp_month: "12".into(),
            exp_year: "28".into(),
            cvv: "123".into(),
            merchant: canonical_merchant,
            spending_limit_usd,
            currency: currency.unwrap_or("USD").to_uppercase(),
            status,
            created_at: now,
        };

        let mut cards = self.read_raw()?;
        cards.insert(card.id.clone(), card.clone());
        self.write_raw(&cards)?;

        Ok(card)
    }

    /// Retrieve card by ID.
    pub fn get_card(&self, card_id: &str) -> Result<Card> {
        let cards = self.read_raw()?;
        cards
            .get(card_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))
    }

    /// Approve spending on a pending virtual card.
    /// Transitions PENDING_APPROVAL -> ACTIVE.
    pub fn approve_card(&mut self, card_id: &str) -> Result<Card> {
        let mut cards = self.read_raw()?;
        let card = cards
            .get_mut(card_id)
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))?;

        if matches!(
            card.status,
            CardStatus::Locked | CardStatus::Charged | CardStatus::Expired
        ) {
            bail!(
                "Cannot approve card '{}' in terminal status '{}'",
                card_id,
                card.status
            );
        }

        card.status = CardStatus::Active;
        let updated = card.clone();
        self.write_raw(&cards)?;

        Ok(updated)
    }

    /// Explicitly lock a card so it cannot be used or recharged.
    pub fn lock_card(&mut self, card_id: &str) -> Result<Card> {
        let mut cards = self.read_raw()?;
        let card = cards
            .get_mut(card_id)
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))?;

        card.status = CardStatus::Locked;
        let updated = card.clone();
        self.write_raw(&cards)?;

        Ok(updated)
    }

    /// Simulate charging a card.
    /// Locks/marks card as CHARGED so it cannot be recharged.
    pub fn charge_card(
        &mut self,
        card_id: &str,
        amount_usd: f64,
        merchant: Option<&str>,
    ) -> Result<Card> {
        if amount_usd < 0.0 {
            bail!("Charge amount cannot be negative");
        }

        let mut cards = self.read_raw()?;
        let card = cards
            .get_mut(card_id)
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))?;

        if card.status != CardStatus::Active {
            bail!(
                "Cannot charge card '{}' with status '{}'. Card must be ACTIVE.",
                card_id,
                card.status
            );
        }

        if amount_usd > card.spending_limit_usd {
            bail!(
                "Charge amount ${:.2} exceeds card spending limit of ${:.2}",
                amount_usd,
                card.spending_limit_usd
            );
        }

        if let Some(m) = merchant {
            let norm_m = normalize_domain(m);
            if norm_m != card.merchant {
                bail!(
                    "Merchant mismatch: card is bounded to '{}', charged by '{}'",
                    card.merchant,
                    norm_m
                );
            }
        }

        card.status = CardStatus::Charged;
        let updated = card.clone();
        self.write_raw(&cards)?;

        Ok(updated)
    }

    /// List cards, optionally filtered.
    pub fn list_cards(
        &self,
        merchant_filter: Option<&str>,
        status_filter: Option<CardStatus>,
    ) -> Result<Vec<Card>> {
        let cards = self.read_raw()?;
        let norm_merchant = merchant_filter.map(normalize_domain);

        let mut results = Vec::new();
        for card in cards.into_values() {
            if let Some(ref m) = norm_merchant
                && card.merchant != *m
            {
                continue;
            }
            if let Some(s) = status_filter
                && card.status != s
            {
                continue;
            }
            results.push(card);
        }

        results.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(results)
    }

    /// Build synthetic input commands for form injection.
    pub fn build_injection_commands(
        &self,
        card: &Card,
        submit: bool,
        split_exp: bool,
    ) -> Vec<String> {
        let mut cmds = Vec::new();
        // Type card number
        cmds.push(format!("xdotool type -- '{}'", card.card_number));
        cmds.push("xdotool key Tab".into());

        // Type expiration
        if split_exp {
            cmds.push(format!("xdotool type -- '{}'", card.exp_month));
            cmds.push("xdotool key Tab".into());
            cmds.push(format!("xdotool type -- '{}'", card.exp_year));
        } else {
            cmds.push(format!(
                "xdotool type -- '{}/{}'",
                card.exp_month, card.exp_year
            ));
        }
        cmds.push("xdotool key Tab".into());

        // Type CVV
        cmds.push(format!("xdotool type -- '{}'", card.cvv));

        // Optional submit
        if submit {
            cmds.push("xdotool key Return".into());
        }

        cmds
    }

    /// Inject card details and transition card to LOCKED.
    pub fn inject_card(
        &mut self,
        screen: u32,
        card_id: &str,
        target_container: &str,
        submit: bool,
        split_exp: bool,
    ) -> Result<CardInjectionResult> {
        let card = self.get_card(card_id)?;

        if card.status != CardStatus::Active {
            bail!(
                "Cannot inject card '{}' with status '{}'. Card must be ACTIVE.",
                card_id,
                card.status
            );
        }

        let cmds = self.build_injection_commands(&card, submit, split_exp);

        let masked_pan = card.masked_card_number();
        let card_id_str = card.id.clone();
        let card_merchant = card.merchant.clone();

        // Lock card immediately upon injection
        let locked = self.lock_card(card_id)?;

        Ok(CardInjectionResult {
            status: "injected".into(),
            card_id: card_id_str,
            screen,
            target_container: target_container.into(),
            merchant: card_merchant,
            card_status: locked.status,
            card_number_masked: masked_pan,
            submitted: submit,
            commands: cmds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("reach-card-test-{}", uuid::Uuid::new_v4()));
            let _ = std::fs::create_dir_all(&p);
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_domain_normalization() {
        assert_eq!(normalize_domain("https://amazon.com/dp/123"), "amazon.com");
        assert_eq!(normalize_domain("http://www.google.com:443"), "google.com");
        assert_eq!(normalize_domain("AMAZON.COM"), "amazon.com");
        assert_eq!(normalize_domain("sub.shop.io/checkout"), "sub.shop.io");
    }

    #[test]
    fn test_luhn_algorithm_and_generation() {
        let pan = generate_synthetic_pan("411122");
        assert_eq!(pan.len(), 16);
        assert!(pan.starts_with("411122"));
        assert!(validate_luhn(&pan));

        assert!(validate_luhn("4000000000000002"));
        assert!(!validate_luhn("4000000000000003"));
    }

    #[test]
    fn test_minting_spending_limit_and_approval_threshold() {
        let tmp_dir = TempDir::new();
        let cards_file = tmp_dir.path().join("cards.json");
        let mut engine = AgentCardEngine::new(Some(cards_file));

        // Spending limit <= threshold ($20 <= $25) -> ACTIVE
        let card1 = engine
            .mint_card("amazon.com", 20.0, 25.0, None, None)
            .unwrap();
        assert_eq!(card1.status, CardStatus::Active);
        assert_eq!(card1.merchant, "amazon.com");
        assert_eq!(card1.spending_limit_usd, 20.0);

        // Spending limit > threshold ($35 > $25) -> PENDING_APPROVAL
        let card2 = engine
            .mint_card("https://store.google.com", 35.0, 25.0, None, None)
            .unwrap();
        assert_eq!(card2.status, CardStatus::PendingApproval);
        assert_eq!(card2.merchant, "store.google.com");
    }

    #[test]
    fn test_approval_gate_and_charging() {
        let tmp_dir = TempDir::new();
        let cards_file = tmp_dir.path().join("cards.json");
        let mut engine = AgentCardEngine::new(Some(cards_file));

        let card = engine
            .mint_card("amazon.com", 35.0, 25.0, None, None)
            .unwrap();
        assert_eq!(card.status, CardStatus::PendingApproval);

        // Cannot charge pending card
        let err = engine.charge_card(&card.id, 10.0, None);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("ACTIVE"));

        // Approve card -> ACTIVE
        let approved = engine.approve_card(&card.id).unwrap();
        assert_eq!(approved.status, CardStatus::Active);

        // Charge exceeding limit fails
        let over_err = engine.charge_card(&card.id, 40.0, None);
        assert!(over_err.is_err());
        assert!(over_err.unwrap_err().to_string().contains("exceeds"));

        // Charge within limit succeeds and locks card to CHARGED
        let charged = engine.charge_card(&card.id, 30.0, None).unwrap();
        assert_eq!(charged.status, CardStatus::Charged);

        // Cannot recharge card
        let recharge_err = engine.charge_card(&card.id, 5.0, None);
        assert!(recharge_err.is_err());
    }

    #[test]
    fn test_single_use_injection_locking() {
        let tmp_dir = TempDir::new();
        let cards_file = tmp_dir.path().join("cards.json");
        let mut engine = AgentCardEngine::new(Some(cards_file));

        let card = engine
            .mint_card("amazon.com", 15.0, 25.0, None, None)
            .unwrap();
        assert_eq!(card.status, CardStatus::Active);

        let res = engine
            .inject_card(0, &card.id, "agent-computer", true, false)
            .unwrap();
        assert_eq!(res.status, "injected");
        assert_eq!(res.card_status, CardStatus::Locked);
        assert_eq!(res.card_number_masked, card.masked_card_number());
        assert!(!res.commands.is_empty());

        // Card is now locked: cannot inject again
        let err = engine.inject_card(0, &card.id, "agent-computer", false, false);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("ACTIVE"));
    }
}
