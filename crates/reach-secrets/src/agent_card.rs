//! Agent Card Bounded Spending Engine & Checkout Injector.
//!
//! Provides virtual card minting with merchant bounds and spending caps,
//! approval gate enforcement, single-use locking, and out-of-band checkout
//! injection into desktop sandboxes via synthetic inputs or Reach MCP.

#![allow(clippy::collapsible_if, clippy::too_many_arguments)]

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

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
    Injecting,
    Charged,
    Locked,
    Expired,
}

impl std::fmt::Display for CardStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CardStatus::PendingApproval => write!(f, "PENDING_APPROVAL"),
            CardStatus::Active => write!(f, "ACTIVE"),
            CardStatus::Injecting => write!(f, "INJECTING"),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected_at: Option<i64>,
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
            idempotency_token: self.idempotency_token.clone(),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_token: Option<String>,
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

/// Extract effective Top-Level Domain plus one label (eTLD+1).
pub fn extract_etld_plus_one(domain_or_url: &str) -> String {
    let host = normalize_domain(domain_or_url);
    if host.is_empty() {
        return String::new();
    }
    if host == "localhost" || host == "127.0.0.1" || host == "::1" || host.ends_with(".localhost") {
        return host;
    }

    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return host;
    }

    let known_second_levels = ["co", "com", "org", "net", "edu", "gov", "ac", "ne", "mil"];
    let len = parts.len();
    if len >= 3 && known_second_levels.contains(&parts[len - 2]) && parts[len - 1].len() == 2 {
        return parts[len - 3..].join(".");
    }

    parts[len - 2..].join(".")
}

/// Validate active page URL against bound target domain.
pub fn validate_origin(active_url: &str, bound_domain: &str) -> Result<()> {
    if active_url.trim().is_empty() {
        bail!("Cannot verify origin: active tab URL is empty or missing");
    }

    let raw = active_url.trim().to_lowercase();
    let (scheme, host_with_port) = if let Some(idx) = raw.find("://") {
        (&raw[..idx], &raw[idx + 3..])
    } else {
        ("https", raw.as_str())
    };

    let host = host_with_port
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");

    let is_localhost = host == "localhost"
        || host == "127.0.0.1"
        || host == "::1"
        || host.ends_with(".localhost")
        || host.starts_with("127.");

    if scheme == "http" {
        if !is_localhost {
            bail!(
                "Insecure origin scheme 'http' for non-localhost URL '{active_url}'. Only https is allowed."
            );
        }
    } else if scheme != "https" {
        bail!(
            "Invalid origin scheme '{scheme}' in URL '{active_url}'. Only https (or localhost http) is permitted."
        );
    }

    let active_etld = extract_etld_plus_one(host);
    let bound_etld = extract_etld_plus_one(bound_domain);

    if active_etld != bound_etld {
        bail!(
            "Origin mismatch: active URL '{active_url}' (eTLD+1: '{active_etld}') does not match bound domain '{bound_domain}' (eTLD+1: '{bound_etld}')"
        );
    }

    Ok(())
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

    let seed = std::process::id() as u128
        ^ SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
    let mut state = seed;
    for i in 0..needed {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407 + i as u128);
        let n = ((state >> 64) % 10) as u8;
        rng_digits.push_str(&n.to_string());
    }

    let partial = format!("{prefix}{rng_digits}");
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
    format!("{partial}{check_digit}")
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

// ═══════════════════════════════════════════════════════════
// File Locking Implementation (.cards.lock)
// ═══════════════════════════════════════════════════════════

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[cfg(unix)]
const LOCK_EX: std::os::raw::c_int = 2;
#[cfg(unix)]
const LOCK_UN: std::os::raw::c_int = 8;

static IN_PROCESS_CARD_LOCK: Mutex<()> = Mutex::new(());

/// RAII lock guard holding exclusive access on `.cards.lock`.
pub struct CardLockGuard<'a> {
    #[allow(dead_code)]
    file: File,
    #[allow(dead_code)]
    in_process_guard: std::sync::MutexGuard<'a, ()>,
}

impl Drop for CardLockGuard<'_> {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let fd = self.file.as_raw_fd();
            unsafe {
                flock(fd, LOCK_UN);
            }
        }
    }
}

/// Agent Card Storage and Engine.
pub struct AgentCardEngine {
    pub cards_file: PathBuf,
    pub cards_dir: PathBuf,
}

pub type CardStore = AgentCardEngine;

impl AgentCardEngine {
    /// Load card store from default location or REACH_CARD_PATH.
    pub fn load_from_default() -> Result<Self> {
        Ok(Self::new(None))
    }

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

    /// Path to the file lock preventing concurrent write clobbering.
    pub fn lock_path(&self) -> PathBuf {
        self.cards_dir.join(".cards.lock")
    }

    /// Acquire exclusive lock across processes and threads on `.cards.lock`.
    pub fn acquire_lock(&self) -> Result<CardLockGuard<'_>> {
        self.ensure_dir()?;
        let in_process_guard = IN_PROCESS_CARD_LOCK
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to acquire in-process card lock: {e}"))?;

        let lock_path = self.lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("Failed to open card lock file {:?}", lock_path))?;

        #[cfg(unix)]
        {
            let perm = fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(&lock_path, perm);
            let fd = file.as_raw_fd();
            let res = unsafe { flock(fd, LOCK_EX) };
            if res != 0 {
                bail!("Failed to acquire exclusive flock on {:?}", lock_path);
            }
        }

        Ok(CardLockGuard {
            file,
            in_process_guard,
        })
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

        let mut map = if let Ok(map) = serde_json::from_str::<HashMap<String, Card>>(&content) {
            map
        } else if let Ok(list) = serde_json::from_str::<Vec<Card>>(&content) {
            let mut map = HashMap::new();
            for c in list {
                map.insert(c.id.clone(), c);
            }
            map
        } else {
            bail!("Failed to deserialize cards.json into card map or list");
        };

        // Treat INJECTING on load as LOCKED (burned / invalidated from crash mid-injection)
        for card in map.values_mut() {
            if card.status == CardStatus::Injecting {
                card.status = CardStatus::Locked;
            }
        }

        Ok(map)
    }

    fn write_raw(&self, cards: &HashMap<String, Card>) -> Result<()> {
        self.ensure_dir()?;
        let json_str = serde_json::to_string_pretty(cards)?;

        static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let cnt = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        let temp_file = self.cards_dir.join(format!(
            ".cards_{}_{}_{}.tmp",
            std::process::id(),
            nanos,
            cnt
        ));

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

        let _lock = self.acquire_lock()?;

        let cid = custom_id.map(|s| s.to_string()).unwrap_or_else(|| {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let rand_val = (nanos ^ ((std::process::id() as u128) << 32) ^ (c as u128)) as u64;
            format!("card_{:08x}", rand_val & 0xffff_ffff)
        });

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
            idempotency_token: None,
            injected_at: None,
        };

        let mut cards = self.read_raw()?;
        cards.insert(card.id.clone(), card.clone());
        self.write_raw(&cards)?;

        Ok(card)
    }

    /// Retrieve card by ID.
    pub fn get_card(&self, card_id: &str) -> Result<Card> {
        let _lock = self.acquire_lock()?;
        let cards = self.read_raw()?;
        cards
            .get(card_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))
    }

    /// Approve spending on a pending virtual card.
    /// Transitions PENDING_APPROVAL -> ACTIVE.
    pub fn approve_card(&mut self, card_id: &str) -> Result<Card> {
        let _lock = self.acquire_lock()?;
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
        let _lock = self.acquire_lock()?;
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
        idempotency_key: Option<&str>,
    ) -> Result<Card> {
        if amount_usd < 0.0 {
            bail!("Charge amount cannot be negative");
        }

        let _lock = self.acquire_lock()?;
        let mut cards = self.read_raw()?;
        let card = cards
            .get_mut(card_id)
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))?;

        if card.status == CardStatus::Charged {
            if let Some(key) = idempotency_key {
                if card.idempotency_token.as_deref() == Some(key) {
                    return Ok(card.clone());
                }
            }
        }

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
        if let Some(key) = idempotency_key {
            card.idempotency_token = Some(key.to_string());
        }
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
        let _lock = self.acquire_lock()?;
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
        current_url: Option<&str>,
        has_checkout_form: Option<bool>,
        idempotency_token: Option<&str>,
    ) -> Result<CardInjectionResult> {
        let _lock = self.acquire_lock()?;
        let mut cards = self.read_raw()?;
        let card = cards
            .get_mut(card_id)
            .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))?;

        // Idempotency token check: if already injected with the same token, return cached result
        if let Some(tok) = idempotency_token {
            if card.idempotency_token.as_deref() == Some(tok)
                && matches!(card.status, CardStatus::Injecting | CardStatus::Locked)
            {
                return Ok(CardInjectionResult {
                    status: "already_injected".into(),
                    card_id: card.id.clone(),
                    screen,
                    target_container: target_container.into(),
                    merchant: card.merchant.clone(),
                    card_status: card.status,
                    card_number_masked: card.masked_card_number(),
                    submitted: submit,
                    commands: Vec::new(),
                });
            }
        }

        if card.status != CardStatus::Active {
            bail!(
                "Cannot inject card '{}' with status '{}'. Card must be ACTIVE.",
                card_id,
                card.status
            );
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if let Some(injected_at) = card.injected_at {
            if now - injected_at < 60 {
                bail!(
                    "Double-submit prevented: card '{}' was injected recently at {}",
                    card_id,
                    injected_at
                );
            }
        }

        let active_url = current_url.ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot inject card '{}': active tab URL must be verified before typing secrets",
                card_id
            )
        })?;

        validate_origin(active_url, &card.merchant)?;

        if let Some(form_ok) = has_checkout_form {
            if !form_ok {
                bail!(
                    "Cannot inject card '{}': no checkout form detected on active page '{}'",
                    card_id,
                    active_url
                );
            }
        }

        // Transition ACTIVE -> INJECTING (persisted to disk) BEFORE keystroke commands
        let effective_tok = idempotency_token.map(String::from).unwrap_or_else(|| {
            static TOK_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let c = TOK_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let rand_val = (nanos ^ ((std::process::id() as u128) << 32) ^ (c as u128)) as u64;
            format!("tok_{:08x}", rand_val & 0xffff_ffff)
        });

        card.status = CardStatus::Injecting;
        card.injected_at = Some(now);
        card.idempotency_token = Some(effective_tok);
        let card_to_inject = card.clone();
        self.write_raw(&cards)?;

        let cmds = self.build_injection_commands(&card_to_inject, submit, split_exp);

        let masked_pan = card_to_inject.masked_card_number();
        let card_id_str = card_to_inject.id.clone();
        let card_merchant = card_to_inject.merchant.clone();

        // Transition INJECTING -> LOCKED immediately after typing
        let locked_card = {
            let card_entry = cards
                .get_mut(card_id)
                .ok_or_else(|| anyhow::anyhow!("Card '{}' not found", card_id))?;
            card_entry.status = CardStatus::Locked;
            card_entry.clone()
        };
        self.write_raw(&cards)?;

        Ok(CardInjectionResult {
            status: "injected".into(),
            card_id: card_id_str,
            screen,
            target_container: target_container.into(),
            merchant: card_merchant,
            card_status: locked_card.status,
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
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let p = std::env::temp_dir().join(format!(
                "reach-card-test-{}_{}_{}",
                std::process::id(),
                nanos,
                c
            ));
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
        let err = engine.charge_card(&card.id, 10.0, None, None);
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("ACTIVE"));

        // Approve card -> ACTIVE
        let approved = engine.approve_card(&card.id).unwrap();
        assert_eq!(approved.status, CardStatus::Active);

        // Charge exceeding limit fails
        let over_err = engine.charge_card(&card.id, 40.0, None, None);
        assert!(over_err.is_err());
        assert!(over_err.unwrap_err().to_string().contains("exceeds"));

        // Charge within limit succeeds and locks card to CHARGED
        let charged = engine
            .charge_card(&card.id, 30.0, None, Some("charge-1"))
            .unwrap();
        assert_eq!(charged.status, CardStatus::Charged);

        // Idempotent charge replay with same key succeeds
        let replay = engine
            .charge_card(&card.id, 30.0, None, Some("charge-1"))
            .unwrap();
        assert_eq!(replay.status, CardStatus::Charged);

        // Cannot recharge card with new or missing key
        let recharge_err = engine.charge_card(&card.id, 5.0, None, None);
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
            .inject_card(
                0,
                &card.id,
                "agent-computer",
                true,
                false,
                Some("https://amazon.com/checkout"),
                Some(true),
                None,
            )
            .unwrap();
        assert_eq!(res.status, "injected");
        assert_eq!(res.card_status, CardStatus::Locked);
        assert_eq!(res.card_number_masked, card.masked_card_number());
        assert!(!res.commands.is_empty());

        // Card is now locked: cannot inject again
        let err = engine.inject_card(
            0,
            &card.id,
            "agent-computer",
            false,
            false,
            Some("https://amazon.com/checkout"),
            Some(true),
            None,
        );
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("ACTIVE"));
    }

    #[test]
    fn test_origin_validation_and_form_check() {
        let tmp_dir = TempDir::new();
        let cards_file = tmp_dir.path().join("cards.json");
        let mut engine = AgentCardEngine::new(Some(cards_file));

        let card = engine
            .mint_card("amazon.com", 20.0, 25.0, None, None)
            .unwrap();

        // 1. Phishing domain rejected
        let phish_err = engine.inject_card(
            0,
            &card.id,
            "agent-computer",
            false,
            false,
            Some("https://evil-phish.com/checkout"),
            Some(true),
            None,
        );
        assert!(phish_err.is_err());
        assert!(
            phish_err
                .unwrap_err()
                .to_string()
                .contains("Origin mismatch")
        );

        // 2. Insecure HTTP on non-localhost rejected
        let http_err = engine.inject_card(
            0,
            &card.id,
            "agent-computer",
            false,
            false,
            Some("http://amazon.com/checkout"),
            Some(true),
            None,
        );
        assert!(http_err.is_err());
        assert!(
            http_err
                .unwrap_err()
                .to_string()
                .contains("Insecure origin scheme")
        );

        // 3. Missing checkout form rejected
        let form_err = engine.inject_card(
            0,
            &card.id,
            "agent-computer",
            false,
            false,
            Some("https://amazon.com/blog"),
            Some(false),
            None,
        );
        assert!(form_err.is_err());
        assert!(
            form_err
                .unwrap_err()
                .to_string()
                .contains("no checkout form")
        );

        // 4. Missing active tab URL rejected
        let no_url_err = engine.inject_card(
            0,
            &card.id,
            "agent-computer",
            false,
            false,
            None,
            Some(true),
            None,
        );
        assert!(no_url_err.is_err());
        assert!(
            no_url_err
                .unwrap_err()
                .to_string()
                .contains("active tab URL must be verified")
        );

        // 5. Valid https subdomain matching eTLD+1 succeeds
        let valid_res = engine.inject_card(
            0,
            &card.id,
            "agent-computer",
            false,
            false,
            Some("https://checkout.amazon.com/pay"),
            Some(true),
            None,
        );
        assert!(valid_res.is_ok());
        assert_eq!(valid_res.unwrap().card_status, CardStatus::Locked);
    }

    #[test]
    fn test_injecting_crash_recovery_and_idempotency() {
        let tmp_dir = TempDir::new();
        let cards_file = tmp_dir.path().join("cards.json");

        // Manually write a card in INJECTING state to simulate crash during typing
        let json_crash = serde_json::json!({
            "card_crashed": {
                "id": "card_crashed",
                "card_number": "4111222233334444",
                "exp_month": "12",
                "exp_year": "28",
                "cvv": "123",
                "merchant": "amazon.com",
                "spending_limit_usd": 20.0,
                "currency": "USD",
                "status": "INJECTING",
                "created_at": 1000
            }
        });
        std::fs::write(
            &cards_file,
            serde_json::to_string_pretty(&json_crash).unwrap(),
        )
        .unwrap();

        let mut engine = AgentCardEngine::new(Some(cards_file.clone()));
        let loaded = engine.get_card("card_crashed").unwrap();
        // INJECTING status on load must be treated as LOCKED
        assert_eq!(loaded.status, CardStatus::Locked);

        // Cannot inject card that crashed mid-injection
        let err = engine.inject_card(
            0,
            "card_crashed",
            "agent-computer",
            false,
            false,
            Some("https://amazon.com/checkout"),
            Some(true),
            None,
        );
        assert!(err.is_err());

        // Test idempotency token replay
        let active_card = engine
            .mint_card("amazon.com", 20.0, 25.0, None, Some("card_idem"))
            .unwrap();
        let res1 = engine.inject_card(
            0,
            &active_card.id,
            "agent-computer",
            true,
            false,
            Some("https://amazon.com/checkout"),
            Some(true),
            Some("token_xyz_1"),
        );
        assert!(res1.is_ok());
        assert_eq!(res1.unwrap().status, "injected");

        // Retrying with same idempotency token returns already_injected without double submitting
        let res2 = engine.inject_card(
            0,
            &active_card.id,
            "agent-computer",
            true,
            false,
            Some("https://amazon.com/checkout"),
            Some(true),
            Some("token_xyz_1"),
        );
        assert!(res2.is_ok());
        assert_eq!(res2.unwrap().status, "already_injected");
    }

    #[test]
    fn test_cards_lock_concurrency() {
        let tmp_dir = TempDir::new();
        let cards_file = tmp_dir.path().join("cards.json");
        let engine = AgentCardEngine::new(Some(cards_file.clone()));

        // Ensure lock file exists after lock acquisition
        {
            let _lock = engine.acquire_lock().unwrap();
            assert!(engine.lock_path().exists());
        }

        // Concurrently mint cards across threads to verify no write clobbering
        let mut handles = Vec::new();
        for i in 0..5 {
            let path_clone = cards_file.clone();
            let handle = std::thread::spawn(move || {
                let mut eng = AgentCardEngine::new(Some(path_clone));
                eng.mint_card(
                    &format!("merchant{i}.com"),
                    10.0 + i as f64,
                    25.0,
                    None,
                    None,
                )
                .unwrap()
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }

        let final_engine = AgentCardEngine::new(Some(cards_file));
        let all_cards = final_engine.list_cards(None, None).unwrap();
        // All 5 cards must exist without clobbering!
        assert_eq!(all_cards.len(), 5);
    }
}
