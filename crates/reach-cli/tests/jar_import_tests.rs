#[allow(dead_code)]
#[path = "../src/commands/jar.rs"]
mod jar;

use jar::{ImportArgs, run_import};
use reach_cli::profile::CookieJarService;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "reach-jar-test-{}_{}_{}",
            std::process::id(),
            nanos,
            c
        ));
        let _ = fs::create_dir_all(&p);
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn test_jar_import_playwright_storage_state() {
    let tmp = TempDir::new();
    let jars_dir = tmp.path().join("jars");
    let state_file = tmp.path().join("storage_state.json");

    let sample_storage_state = serde_json::json!({
        "cookies": [
            {
                "name": "gh_sess",
                "value": "sess12345",
                "domain": ".github.com",
                "path": "/",
                "httpOnly": true,
                "secure": true,
                "sameSite": "Lax"
            },
            {
                "name": "oauth_token",
                "value": "token_abc",
                "domain": "api.github.com",
                "path": "/v1",
                "httpOnly": false,
                "secure": true,
                "sameSite": "None"
            },
            {
                "name": "google_pref",
                "value": "pref_xyz",
                "domain": ".google.com",
                "path": "/",
                "httpOnly": false,
                "secure": true,
                "sameSite": "Lax"
            }
        ],
        "origins": [
            {
                "origin": "https://github.com",
                "localStorage": [
                    { "name": "color-mode", "value": "dark" }
                ]
            }
        ]
    });

    fs::write(
        &state_file,
        serde_json::to_string_pretty(&sample_storage_state).unwrap(),
    )
    .unwrap();

    // 1. Import all domains automatically inferred
    let args = ImportArgs {
        file: state_file.clone(),
        domains: None,
    };

    run_import(Some(jars_dir.clone()), args)
        .await
        .expect("Import should succeed");

    let svc = CookieJarService::new(jars_dir.clone());
    let github_jar = svc
        .load_jar("github.com")
        .expect("github.com jar must exist");
    assert_eq!(github_jar.jar_version, 1);
    assert_eq!(github_jar.cookies.len(), 2);
    assert!(github_jar.cookies.iter().any(|c| c.name == "gh_sess"));
    assert!(github_jar.cookies.iter().any(|c| c.name == "oauth_token"));

    let google_jar = svc
        .load_jar("google.com")
        .expect("google.com jar must exist");
    assert_eq!(google_jar.jar_version, 1);
    assert_eq!(google_jar.cookies.len(), 1);
    assert_eq!(google_jar.cookies[0].name, "google_pref");

    // 2. Subsequent import must bump jar_version to 2
    let args2 = ImportArgs {
        file: state_file.clone(),
        domains: Some(vec!["github.com".into()]),
    };

    run_import(Some(jars_dir.clone()), args2)
        .await
        .expect("Second import should succeed and bump version");

    let updated_github_jar = svc.load_jar("github.com").unwrap();
    assert_eq!(updated_github_jar.jar_version, 2);

    // Google jar untouched
    let untouched_google_jar = svc.load_jar("google.com").unwrap();
    assert_eq!(untouched_google_jar.jar_version, 1);
}

#[tokio::test]
async fn test_jar_import_cookie_array_and_sanitization() {
    let tmp = TempDir::new();
    let jars_dir = tmp.path().join("jars");
    let state_file = tmp.path().join("cookies_only.json");

    let raw_cookies = serde_json::json!([
        {
            "name": "session",
            "value": "abc",
            "domain": "HTTP://EXAMPLE.COM:8080/path",
            "path": "/"
        }
    ]);

    fs::write(
        &state_file,
        serde_json::to_string_pretty(&raw_cookies).unwrap(),
    )
    .unwrap();

    let args = ImportArgs {
        file: state_file,
        domains: None,
    };

    run_import(Some(jars_dir.clone()), args)
        .await
        .expect("Import with raw cookies array should succeed");

    let svc = CookieJarService::new(jars_dir);
    let jar = svc
        .load_jar("example.com")
        .expect("Domain should be sanitized to example.com");
    assert_eq!(jar.jar_version, 1);
    assert_eq!(jar.cookies.len(), 1);
    assert_eq!(jar.cookies[0].name, "session");
}
