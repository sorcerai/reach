//! Native secret vault and RFC 6238 TOTP engine.
//!
//! Provides POSIX-restricted credential storage (0700 dir, 0600 file) in `~/.reach/vault/secrets.json`
//! (or path configured via `REACH_VAULT_PATH` or `config.toml`), Unix permission enforcement
//! (0700 dir, 0600 file), domain normalization, and standard Base32 / HMAC-SHA1 TOTP generation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════
// Error Types
// ═══════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum VaultError {
    DomainNotFound(String),
    NoTotpSecret(String),
    InvalidBase32(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    SystemTime(std::time::SystemTimeError),
    Other(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::DomainNotFound(d) => {
                write!(f, "credentials for domain '{d}' not found in vault")
            }
            VaultError::NoTotpSecret(d) => {
                write!(f, "no TOTP secret configured for domain '{d}'")
            }
            VaultError::InvalidBase32(msg) => write!(f, "invalid Base32 secret: {msg}"),
            VaultError::Io(err) => write!(f, "vault I/O error: {err}"),
            VaultError::Json(err) => write!(f, "vault JSON error: {err}"),
            VaultError::SystemTime(err) => write!(f, "system time error: {err}"),
            VaultError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for VaultError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VaultError::Io(err) => Some(err),
            VaultError::Json(err) => Some(err),
            VaultError::SystemTime(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for VaultError {
    fn from(err: std::io::Error) -> Self {
        VaultError::Io(err)
    }
}

impl From<serde_json::Error> for VaultError {
    fn from(err: serde_json::Error) -> Self {
        VaultError::Json(err)
    }
}

impl From<std::time::SystemTimeError> for VaultError {
    fn from(err: std::time::SystemTimeError) -> Self {
        VaultError::SystemTime(err)
    }
}

// ═══════════════════════════════════════════════════════════
// Data Structures
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totp_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DomainSummary {
    pub domain: String,
    pub username: String,
    pub has_totp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VaultData {
    #[serde(default)]
    pub credentials: BTreeMap<String, Credential>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredData {
    Structured(VaultData),
    Flat(BTreeMap<String, Credential>),
}

// ═══════════════════════════════════════════════════════════
// Domain Normalization
// ═══════════════════════════════════════════════════════════

/// Normalizes domain identifiers by stripping schemes, userinfo, ports, paths,
/// queries, fragments, and leading 'www.' prefixes.
///
/// Example: `https://user:pass@www.github.com:443/login?ref=1#top` -> `github.com`
pub fn normalize_domain(input: &str) -> String {
    let mut s = input.trim();

    // Strip scheme if present (e.g. "https://", "http://", "ftp://")
    if let Some(pos) = s.find("://") {
        s = &s[pos + 3..];
    } else if let Some(stripped) = s.strip_prefix("//") {
        s = stripped;
    }

    // Strip path, query, and fragment (e.g. "/login", "?foo=bar", "#fragment")
    if let Some(pos) = s.find(['/', '?', '#']) {
        s = &s[..pos];
    }

    // Strip user info if present (e.g. "user:pass@host")
    if let Some(pos) = s.rfind('@') {
        s = &s[pos + 1..];
    }

    // Strip port if present (e.g. "github.com:443" or "[::1]:8080")
    if s.starts_with('[') {
        if let Some(end_bracket) = s.find(']') {
            s = &s[..=end_bracket];
        }
    } else if let Some(pos) = s.rfind(':') {
        let port_candidate = &s[pos + 1..];
        if port_candidate.chars().all(|c| c.is_ascii_digit()) && !port_candidate.is_empty() {
            s = &s[..pos];
        }
    }

    let mut lower = s.to_ascii_lowercase();

    // Strip trailing DNS dots
    while lower.ends_with('.') {
        lower.pop();
    }

    // Strip leading "www."
    while let Some(stripped) = lower.strip_prefix("www.") {
        lower = stripped.to_string();
    }

    lower
}

// ═══════════════════════════════════════════════════════════
// Base32 Decoding (RFC 4648)
// ═══════════════════════════════════════════════════════════

/// Decodes standard RFC 4648 Base32 strings, handling unpadded strings,
/// lowercase letters, hyphens, and whitespace.
pub fn decode_base32(input: &str) -> Result<Vec<u8>, VaultError> {
    let clean: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();

    let clean = clean.trim_end_matches('=');
    if clean.is_empty() {
        return Err(VaultError::InvalidBase32("empty secret".into()));
    }

    let mut bits = 0u32;
    let mut num_bits = 0usize;
    let mut bytes = Vec::new();

    for c in clean.chars() {
        let val = match c {
            'A'..='Z' => (c as u8 - b'A') as u32,
            'a'..='z' => (c as u8 - b'a') as u32,
            '2'..='7' => (c as u8 - b'2' + 26) as u32,
            _ => {
                return Err(VaultError::InvalidBase32(format!(
                    "invalid character: '{c}'"
                )));
            }
        };

        bits = (bits << 5) | val;
        num_bits += 5;

        if num_bits >= 8 {
            num_bits -= 8;
            bytes.push(((bits >> num_bits) & 0xFF) as u8);
        }
    }

    Ok(bytes)
}

// ═══════════════════════════════════════════════════════════
// Pure Rust HMAC-SHA1 and RFC 6238 TOTP
// ═══════════════════════════════════════════════════════════

/// Pure Rust implementation of SHA-1 (RFC 3174 / FIPS PUB 180-1).
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, item) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*item);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

/// Pure Rust implementation of HMAC-SHA1 (RFC 2104).
pub fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let hashed = sha1(key);
        k[..20].copy_from_slice(&hashed);
    } else {
        k[..key.len()].copy_from_slice(key);
    }

    let mut inner_input = Vec::with_capacity(64 + message.len());
    for b in &k {
        inner_input.push(b ^ 0x36);
    }
    inner_input.extend_from_slice(message);
    let inner_hash = sha1(&inner_input);

    let mut outer_input = Vec::with_capacity(64 + 20);
    for b in &k {
        outer_input.push(b ^ 0x5c);
    }
    outer_input.extend_from_slice(&inner_hash);
    sha1(&outer_input)
}

/// HOTP dynamic truncation algorithm (RFC 4226).
pub fn hotp(key: &[u8], counter: u64) -> u32 {
    let counter_bytes = counter.to_be_bytes();
    let hash = hmac_sha1(key, &counter_bytes);
    let offset = (hash[19] & 0x0f) as usize;
    let binary_code = (((hash[offset] & 0x7f) as u32) << 24)
        | ((hash[offset + 1] as u32) << 16)
        | ((hash[offset + 2] as u32) << 8)
        | (hash[offset + 3] as u32);
    binary_code % 1_000_000
}

/// Computes a 6-digit TOTP token at the given timestamp and time step (default 30s).
pub fn totp_at(key: &[u8], timestamp_secs: u64, step_secs: u64) -> String {
    let time_step = timestamp_secs / step_secs;
    let code = hotp(key, time_step);
    format!("{code:06}")
}

/// Generates a 6-digit TOTP token from a Base32 secret at an explicit Unix timestamp.
pub fn generate_totp_from_secret(secret: &str, timestamp_secs: u64) -> Result<String, VaultError> {
    let key = decode_base32(secret)?;
    Ok(totp_at(&key, timestamp_secs, 30))
}

/// Generates a 6-digit TOTP token from a Base32 secret at the current system time.
pub fn totp_now(secret: &str) -> Result<String, VaultError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    generate_totp_from_secret(secret, now)
}

// ═══════════════════════════════════════════════════════════
// Permissions Enforcement
// ═══════════════════════════════════════════════════════════

#[cfg(unix)]
pub fn enforce_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if path == Path::new("/tmp")
        || path == Path::new("/var/tmp")
        || path == Path::new("/")
        || path == Path::new("/private/tmp")
        || path == Path::new("/private/var/tmp")
    {
        return Ok(());
    }
    let perms = std::fs::Permissions::from_mode(0o700);
    match std::fs::set_permissions(path, perms) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(not(unix))]
pub fn enforce_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn enforce_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
pub fn enforce_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

// ═══════════════════════════════════════════════════════════
// Vault Engine
// ═══════════════════════════════════════════════════════════

/// Represents the native secret vault instance operating at a specific file path.
#[derive(Debug, Clone)]
pub struct Vault {
    path: PathBuf,
}

impl Vault {
    /// Create a new `Vault` pointing to the specified path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves the default vault file path based on `REACH_VAULT_PATH` env,
    /// `config.toml`, or fallback to `~/.reach/vault/secrets.json`.
    pub fn default_path() -> PathBuf {
        if let Ok(env_val) = std::env::var("REACH_VAULT_PATH") {
            let trimmed = env_val.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed);
            }
        }
        let cfg = crate::config::ReachConfig::load();
        if let Some(p) = cfg.vault.path {
            return p;
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".reach")
            .join("vault")
            .join("secrets.json")
    }

    /// Open vault with the default path.
    pub fn open_default() -> Self {
        Self::new(Self::default_path())
    }

    /// Returns the active path of this vault.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load vault data from disk, enforcing Unix permissions on read.
    pub fn load_data(&self) -> Result<VaultData, VaultError> {
        if !self.path.exists() {
            return Ok(VaultData::default());
        }

        let _ = enforce_file_permissions(&self.path);

        let content = std::fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            return Ok(VaultData::default());
        }

        match serde_json::from_str::<StoredData>(&content) {
            Ok(StoredData::Structured(v)) => Ok(v),
            Ok(StoredData::Flat(m)) => Ok(VaultData { credentials: m }),
            Err(e) => Err(VaultError::Json(e)),
        }
    }

    /// Save vault data to disk, enforcing 0700 dir permissions and 0600 file permissions.
    pub fn save_data(&self, data: &VaultData) -> Result<(), VaultError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            enforce_dir_permissions(parent)?;
        }

        let json_str = serde_json::to_string_pretty(data)?;

        #[cfg(unix)]
        {
            use std::fs::OpenOptions;
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;

            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
            file.write_all(json_str.as_bytes())?;
            file.flush()?;
            enforce_file_permissions(&self.path)?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&self.path, json_str)?;
        }

        Ok(())
    }

    /// Store a credential for a domain, normalizing the domain first.
    pub fn set(
        &self,
        domain: &str,
        username: &str,
        password: &str,
        totp_secret: Option<&str>,
    ) -> Result<(), VaultError> {
        let normalized = normalize_domain(domain);
        if normalized.is_empty() {
            return Err(VaultError::Other("domain cannot be empty".into()));
        }

        let clean_secret = if let Some(s) = totp_secret {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                // Validate base32 decoding
                decode_base32(trimmed)?;
                Some(trimmed.to_string())
            } else {
                None
            }
        } else {
            None
        };

        let mut data = self.load_data()?;
        data.credentials.insert(
            normalized,
            Credential {
                username: username.to_string(),
                password: password.to_string(),
                totp_secret: clean_secret,
            },
        );
        self.save_data(&data)
    }

    /// Retrieve credentials for a domain.
    pub fn get(&self, domain: &str) -> Option<Credential> {
        let normalized = normalize_domain(domain);
        if normalized.is_empty() {
            return None;
        }
        let data = self.load_data().ok()?;
        data.credentials.get(&normalized).cloned()
    }

    /// List all domains stored in the vault.
    pub fn list(&self) -> Vec<DomainSummary> {
        let data = self.load_data().unwrap_or_default();
        data.credentials
            .into_iter()
            .map(|(domain, cred)| DomainSummary {
                domain,
                username: cred.username,
                has_totp: cred.totp_secret.is_some(),
            })
            .collect()
    }

    /// Delete credentials for a domain. Returns true if removed, false otherwise.
    pub fn delete(&self, domain: &str) -> bool {
        let normalized = normalize_domain(domain);
        if normalized.is_empty() {
            return false;
        }
        let mut data = match self.load_data() {
            Ok(d) => d,
            Err(_) => return false,
        };
        if data.credentials.remove(&normalized).is_some() {
            let _ = self.save_data(&data);
            true
        } else {
            false
        }
    }

    /// Generate a 6-digit TOTP token for the specified domain at current time.
    pub fn generate_totp(&self, domain: &str) -> Result<String, VaultError> {
        let normalized = normalize_domain(domain);
        if normalized.is_empty() {
            return Err(VaultError::DomainNotFound(domain.to_string()));
        }
        let cred = self
            .get(&normalized)
            .ok_or_else(|| VaultError::DomainNotFound(normalized.clone()))?;
        let secret = cred
            .totp_secret
            .ok_or(VaultError::NoTotpSecret(normalized))?;
        totp_now(&secret)
    }
}

// ═══════════════════════════════════════════════════════════
// Module-level Convenience Functions
// ═══════════════════════════════════════════════════════════

pub fn set(
    domain: &str,
    username: &str,
    password: &str,
    totp_secret: Option<&str>,
) -> Result<(), VaultError> {
    Vault::open_default().set(domain, username, password, totp_secret)
}

pub fn get(domain: &str) -> Option<Credential> {
    Vault::open_default().get(domain)
}

pub fn list() -> Vec<DomainSummary> {
    Vault::open_default().list()
}

pub fn delete(domain: &str) -> bool {
    Vault::open_default().delete(domain)
}

pub fn generate_totp(domain: &str) -> Result<String, VaultError> {
    Vault::open_default().generate_totp(domain)
}

// ═══════════════════════════════════════════════════════════
// Unit Tests
// ═══════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_normalization() {
        assert_eq!(normalize_domain("https://github.com/login"), "github.com");
        assert_eq!(
            normalize_domain("http://www.google.com:8080/search?q=test#hash"),
            "google.com"
        );
        assert_eq!(normalize_domain("WWW.EXAMPLE.COM"), "example.com");
        assert_eq!(normalize_domain("www.facebook.com"), "facebook.com");
        assert_eq!(normalize_domain("github.com"), "github.com");
        assert_eq!(normalize_domain("github.com."), "github.com");
        assert_eq!(
            normalize_domain("https://sub.domain.co.uk/path/to/resource?a=1&b=2"),
            "sub.domain.co.uk"
        );
        assert_eq!(
            normalize_domain("http://user:pass@www.gitlab.com:8443/auth"),
            "gitlab.com"
        );
        assert_eq!(normalize_domain("  https://github.com/  "), "github.com");
        assert_eq!(normalize_domain("http://localhost:3000/"), "localhost");
        assert_eq!(normalize_domain("http://[::1]:8080/path"), "[::1]");
    }

    #[test]
    fn test_base32_decoding() {
        // "12345678901234567890" in Base32
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"; // gitleaks:allow
        let decoded = decode_base32(secret).unwrap();
        assert_eq!(decoded, b"12345678901234567890");

        // Lowercase and whitespace / hyphens
        let noisy = "  gezd gnbv - gy3t qojq - gezd gnbv - gy3t qojq == ";
        let decoded_noisy = decode_base32(noisy).unwrap();
        assert_eq!(decoded_noisy, b"12345678901234567890");

        // RFC 4648 test vector: "foobar" -> "MZXW6YTBOI======"
        assert_eq!(decode_base32("MZXW6YTBOI======").unwrap(), b"foobar");
        assert_eq!(decode_base32("MZXW6YTBOI").unwrap(), b"foobar");

        // Invalid characters
        assert!(matches!(
            decode_base32("GEZD8NBV"),
            Err(VaultError::InvalidBase32(_))
        ));
    }

    #[test]
    fn test_rfc6238_official_test_vectors() {
        // From RFC 6238 Appendix B: Test Vectors for SHA1
        // Secret in ASCII: "12345678901234567890"
        // Base32: "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
        let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"; // gitleaks:allow

        // RFC 6238 Table 1 (SHA1 8-digit codes: the last 6 digits match 6-digit TOTP)
        // Time = 59s: 8-digit = 94287082 -> 6-digit = 287082
        assert_eq!(generate_totp_from_secret(secret, 59).unwrap(), "287082");

        // Time = 1111111109s: 8-digit = 07081804 -> 6-digit = 081804
        assert_eq!(
            generate_totp_from_secret(secret, 1_111_111_109).unwrap(),
            "081804"
        );

        // Time = 1111111111s: 8-digit = 14050471 -> 6-digit = 050471
        assert_eq!(
            generate_totp_from_secret(secret, 1_111_111_111).unwrap(),
            "050471"
        );

        // Time = 1234567890s: 8-digit = 89005924 -> 6-digit = 005924 (verifies zero-padding)
        assert_eq!(
            generate_totp_from_secret(secret, 1_234_567_890).unwrap(),
            "005924"
        );

        // Time = 2000000000s: 8-digit = 69279037 -> 6-digit = 279037
        assert_eq!(
            generate_totp_from_secret(secret, 2_000_000_000).unwrap(),
            "279037"
        );

        // Time = 20000000000s: 8-digit = 65353130 -> 6-digit = 353130
        assert_eq!(
            generate_totp_from_secret(secret, 20_000_000_000).unwrap(),
            "353130"
        );
    }

    #[test]
    fn test_rfc4226_hotp_vectors() {
        // RFC 4226 Section 5.4 values for key = "12345678901234567890"
        let key = b"12345678901234567890";
        let expected = [
            755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (counter, expected_code) in expected.iter().enumerate() {
            assert_eq!(hotp(key, counter as u64), *expected_code);
        }
    }

    #[test]
    fn test_vault_persistence_and_permissions() {
        let unique = format!("reach-test-vault-{}", uuid::Uuid::new_v4());
        let temp_dir = std::env::temp_dir().join(unique);
        let vault_file = temp_dir.join("secrets.json");
        let vault = Vault::new(&vault_file);

        // Initially empty
        assert_eq!(vault.list(), vec![]);
        assert_eq!(vault.get("github.com"), None);

        // Insert credentials
        vault
            .set(
                "https://github.com/login",
                "octocat",
                "secret-pass",
                Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
            )
            .expect("set failed");

        // Verify retrieval with domain normalization
        let cred = vault.get("github.com").expect("should find domain");
        assert_eq!(cred.username, "octocat");
        assert_eq!(cred.password, "secret-pass");
        assert_eq!(
            cred.totp_secret.as_deref(),
            Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")
        );

        // Verify list
        let summaries = vault.list();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].domain, "github.com");
        assert_eq!(summaries[0].username, "octocat");
        assert!(summaries[0].has_totp);

        // Generate TOTP for stored domain
        let totp = vault.generate_totp("github.com").expect("totp failed");
        assert_eq!(totp.len(), 6);
        assert!(totp.chars().all(|c| c.is_ascii_digit()));

        // Check Unix permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_meta = std::fs::metadata(&temp_dir).unwrap();
            assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);

            let file_meta = std::fs::metadata(&vault_file).unwrap();
            assert_eq!(file_meta.permissions().mode() & 0o777, 0o600);
        }

        // Delete credential
        assert!(vault.delete("github.com"));
        assert!(!vault.delete("github.com"));
        assert_eq!(vault.get("github.com"), None);
        assert_eq!(vault.list().len(), 0);

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
