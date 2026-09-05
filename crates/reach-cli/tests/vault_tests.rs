//! Unit and integration tests for native secret vault and TOTP engine.

use reach_cli::vault::*;
use std::path::PathBuf;

#[test]
fn test_domain_normalization_comprehensive() {
    let cases = vec![
        ("https://github.com/login", "github.com"),
        ("http://github.com", "github.com"),
        ("https://www.github.com/login", "github.com"),
        (
            "http://www.google.com:8080/search?q=test#hash",
            "google.com",
        ),
        ("WWW.GOOGLE.COM", "google.com"),
        ("www.facebook.com/", "facebook.com"),
        (
            "http://user:pass@www.gitlab.com:8443/auth/callback",
            "gitlab.com",
        ),
        (
            "https://sub.domain.co.uk/path/to/page?a=1&b=2",
            "sub.domain.co.uk",
        ),
        ("  https://github.com/  ", "github.com"),
        ("github.com.", "github.com"),
        ("http://localhost:3000/dashboard", "localhost"),
        ("http://[::1]:8080/metrics", "[::1]"),
        ("//api.slack.com/methods", "api.slack.com"),
        ("ftp://ftp.is.co.za/rfc/rfc1808.txt", "ftp.is.co.za"),
    ];

    for (raw, expected) in cases {
        assert_eq!(
            normalize_domain(raw),
            expected,
            "failed normalizing '{}'",
            raw
        );
    }
}

#[test]
fn test_base32_decoding_variants() {
    // RFC 4648 test vectors
    assert_eq!(decode_base32("MY======").unwrap(), b"f");
    assert_eq!(decode_base32("MZXQ====").unwrap(), b"fo");
    assert_eq!(decode_base32("MZXW6===").unwrap(), b"foo");
    assert_eq!(decode_base32("MZXW6YQ=").unwrap(), b"foob");
    assert_eq!(decode_base32("MZXW6YTBOI======").unwrap(), b"foobar");

    // Unpadded test vectors
    assert_eq!(decode_base32("MY").unwrap(), b"f");
    assert_eq!(decode_base32("MZXQ").unwrap(), b"fo");
    assert_eq!(decode_base32("MZXW6").unwrap(), b"foo");
    assert_eq!(decode_base32("MZXW6YQ").unwrap(), b"foob");
    assert_eq!(decode_base32("MZXW6YTBOI").unwrap(), b"foobar");

    // Lowercase
    assert_eq!(decode_base32("mzxw6ytboi").unwrap(), b"foobar");

    // Whitespace and hyphens
    assert_eq!(decode_base32("  MZXW - 6YTB - OI  ").unwrap(), b"foobar");

    // Invalid Base32 characters (0, 1, 8, 9 are not in RFC 4648 Base32 alphabet)
    assert!(decode_base32("MZXW8").is_err());
    assert!(decode_base32("MZXW0").is_err());
    assert!(decode_base32("MZXW1").is_err());
    assert!(decode_base32("MZXW9").is_err());
    assert!(decode_base32("!@#$%^").is_err());
}

#[test]
fn test_rfc6238_official_test_table_vectors() {
    // From RFC 6238 Appendix B: Test Vectors for SHA1
    // Shared secret: "12345678901234567890" (ASCII)
    // Base32: "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"
    let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"; // gitleaks:allow

    let test_vectors = vec![
        (59u64, "287082"),
        (1_111_111_109u64, "081804"),
        (1_111_111_111u64, "050471"),
        (1_234_567_890u64, "005924"), // leading zeros verification
        (2_000_000_000u64, "279037"),
        (20_000_000_000u64, "353130"),
    ];

    for (timestamp, expected_code) in test_vectors {
        let code = generate_totp_from_secret(secret, timestamp)
            .unwrap_or_else(|_| panic!("failed generating TOTP for time {timestamp}"));
        assert_eq!(
            code, expected_code,
            "mismatch at timestamp {timestamp}: expected {expected_code}, got {code}"
        );
    }
}

#[test]
fn test_vault_persistence_lifecycle() {
    let temp_dir = std::env::temp_dir().join(format!("reach-vault-test-{}", uuid::Uuid::new_v4()));
    let file_path = temp_dir.join("secrets.json");
    let vault = Vault::new(&file_path);

    // 1. Initial state
    assert!(vault.list().is_empty());
    assert!(vault.get("github.com").is_none());

    // 2. Set credentials
    vault
        .set(
            "https://github.com/login",
            "testuser",
            "testpassword",
            Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
        )
        .unwrap();

    // 3. Verify normalization lookup
    let cred = vault.get("GITHUB.COM").expect("should find normalized");
    assert_eq!(cred.username, "testuser");
    assert_eq!(cred.password, "testpassword");
    assert_eq!(
        cred.totp_secret.as_deref(),
        Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")
    );

    // 4. Update credentials
    vault
        .set("github.com", "updateduser", "newpassword", None)
        .unwrap();
    let updated = vault.get("github.com").expect("should find updated");
    assert_eq!(updated.username, "updateduser");
    assert_eq!(updated.password, "newpassword");
    assert!(updated.totp_secret.is_none());

    // 5. Add second domain
    vault
        .set(
            "https://gitlab.com",
            "gituser",
            "gitpass",
            Some("MZXW6YTBOI"),
        )
        .unwrap();

    let list = vault.list();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].domain, "github.com");
    assert!(!list[0].has_totp);
    assert_eq!(list[1].domain, "gitlab.com");
    assert!(list[1].has_totp);

    // 6. Verify Unix permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_meta = std::fs::metadata(&temp_dir).unwrap();
        assert_eq!(
            dir_meta.permissions().mode() & 0o777,
            0o700,
            "directory permissions must be 0700"
        );

        let file_meta = std::fs::metadata(&file_path).unwrap();
        assert_eq!(
            file_meta.permissions().mode() & 0o777,
            0o600,
            "file permissions must be 0600"
        );
    }

    // 7. Delete operations
    assert!(vault.delete("https://www.gitlab.com"));
    assert_eq!(vault.list().len(), 1);
    assert!(!vault.delete("gitlab.com")); // second delete returns false

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_reach_vault_path_env_override() {
    let custom_path = PathBuf::from("/tmp/custom-vault/my-secrets.json");
    unsafe {
        std::env::set_var("REACH_VAULT_PATH", &custom_path);
    }
    assert_eq!(Vault::default_path(), custom_path);
    unsafe {
        std::env::remove_var("REACH_VAULT_PATH");
    }
}
