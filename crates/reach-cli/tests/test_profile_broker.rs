use axum::http::StatusCode;
use axum::response::IntoResponse;
use reach_cli::profile::{
    Cookie, CookieJarService, LocalStorageItem, LockHolderInfo, OriginState, ProfileBroker,
    ProfileLockError, StorageState,
};
use std::collections::HashMap;

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("reach-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
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
fn test_lock_acquisition_and_mutual_exclusion() {
    let tmp_dir = TempDir::new();
    let broker = ProfileBroker::new(tmp_dir.path().to_path_buf());

    assert!(!broker.is_locked("test-profile-1"));
    assert_eq!(broker.holder_info("test-profile-1"), None);

    let holder1 = LockHolderInfo::new(Some(0), Some("task-a".into()), Some("agent-1".into()));
    let lease1 = broker
        .acquire_with_holder("test-profile-1", 0, Some(holder1.clone()))
        .expect("first acquire should succeed");

    assert_eq!(lease1.profile(), "test-profile-1");
    assert_eq!(lease1.holder().task.as_deref(), Some("task-a"));
    assert_eq!(lease1.holder().owner.as_deref(), Some("agent-1"));
    assert!(broker.is_locked("test-profile-1"));

    // Verify lock is in host locks dir and NOT in profile dir
    assert!(!lease1.profile_dir().join(".reach.lock").exists());
    assert_eq!(lease1.lock_path(), broker.lock_path_for("test-profile-1"));
    assert!(lease1.lock_path().starts_with(broker.locks_dir()));
    assert!(lease1.lock_path().exists());

    let holder_query = broker.holder_info("test-profile-1");
    assert!(holder_query.is_some());
    assert_eq!(holder_query.unwrap().lease_id, holder1.lease_id);

    // Second acquire on the same profile must fail immediately
    let holder2 = LockHolderInfo::new(Some(1), Some("task-b".into()), Some("agent-2".into()));
    let second_res = broker.acquire_with_holder("test-profile-1", 0, Some(holder2));

    match second_res {
        Err(ProfileLockError::Locked { profile, holder }) => {
            assert_eq!(profile, "test-profile-1");
            let h = holder.expect("holder info must be returned on lock conflict");
            assert_eq!(h.lease_id, holder1.lease_id);
            assert_eq!(h.task.as_deref(), Some("task-a"));
        }
        other => panic!("expected ProfileLockError::Locked, got {other:?}"),
    }

    // Acquire with timeout should also fail with Timeout error
    let timeout_res = broker.acquire("test-profile-1", 50);
    match timeout_res {
        Err(ProfileLockError::Timeout {
            profile,
            timeout_ms,
            holder,
        }) => {
            assert_eq!(profile, "test-profile-1");
            assert_eq!(timeout_ms, 50);
            assert_eq!(holder.unwrap().lease_id, holder1.lease_id);
        }
        other => panic!("expected ProfileLockError::Timeout, got {other:?}"),
    }
}

#[test]
fn test_lock_release_on_drop() {
    let tmp_dir = TempDir::new();
    let broker = ProfileBroker::new(tmp_dir.path().to_path_buf());

    {
        let lease = broker
            .acquire("scoped-profile", 0)
            .expect("initial acquire should succeed");
        assert!(broker.is_locked("scoped-profile"));
        assert_eq!(lease.profile(), "scoped-profile");

        // Competing acquire fails while held
        assert!(broker.acquire("scoped-profile", 0).is_err());
    }
    // `lease` is dropped here

    // After drop, profile lock must be released
    assert!(!broker.is_locked("scoped-profile"));

    // New acquire should now succeed cleanly
    let lease2 = broker
        .acquire("scoped-profile", 0)
        .expect("acquire after drop must succeed");
    assert!(broker.is_locked("scoped-profile"));
    drop(lease2);

    assert!(!broker.is_locked("scoped-profile"));
}

#[test]
fn test_ephemeral_context_profile_locking() {
    let tmp_dir = TempDir::new();
    let broker = ProfileBroker::new(tmp_dir.path().to_path_buf());

    let ephemeral_dir = tmp_dir.path().join(format!("ctx-{}", uuid::Uuid::new_v4()));
    let ephemeral_path_str = ephemeral_dir.to_str().unwrap().to_string();

    let lease = broker
        .acquire(&ephemeral_path_str, 0)
        .expect("ephemeral context acquire must succeed");

    assert!(broker.is_locked(&ephemeral_path_str));

    // Competing acquire on same ephemeral path fails
    let comp = broker.acquire(&ephemeral_path_str, 0);
    assert!(matches!(comp, Err(ProfileLockError::Locked { .. })));

    drop(lease);
    assert!(!broker.is_locked(&ephemeral_path_str));
}

#[test]
fn test_cookie_jar_serialization_concurrency_and_hydration() {
    let tmp_dir = TempDir::new();
    let jars = CookieJarService::new(tmp_dir.path().to_path_buf());

    // 1. Initial state is None
    assert_eq!(jars.load_jar("github.com"), None);

    // 2. Save initial state (version 0 -> becomes version 1)
    let initial_cookie = Cookie {
        name: "user_session".into(),
        value: "secret123".into(),
        domain: ".github.com".into(),
        path: "/".into(),
        expires: Some(1800000000.0),
        http_only: Some(true),
        secure: Some(true),
        same_site: Some("Lax".into()),
        extra: HashMap::new(),
    };
    let initial_origin = OriginState {
        origin: "https://github.com".into(),
        local_storage: vec![LocalStorageItem {
            name: "theme".into(),
            value: "dark".into(),
        }],
    };
    let state = StorageState {
        jar_version: 0,
        cookies: vec![initial_cookie.clone()],
        origins: vec![initial_origin.clone()],
    };

    jars.save_jar("github.com", &state)
        .expect("save_jar should succeed");

    let loaded = jars
        .load_jar("github.com")
        .expect("load_jar should find saved jar");
    assert_eq!(loaded.jar_version, 1);
    assert_eq!(loaded.cookies.len(), 1);
    assert_eq!(loaded.cookies[0].name, "user_session");
    assert_eq!(loaded.cookies[0].value, "secret123");
    assert_eq!(loaded.origins.len(), 1);
    assert_eq!(loaded.origins[0].local_storage[0].value, "dark");

    // 3. Optimistic concurrency detection:
    // Trying to save with stale version (0) when disk is version 1 must error
    let stale_state = StorageState {
        jar_version: 0,
        cookies: vec![],
        origins: vec![],
    };
    let conflict_err = jars.save_jar("github.com", &stale_state);
    assert!(
        conflict_err.is_err(),
        "optimistic concurrency must reject stale version"
    );
    let err_msg = conflict_err.unwrap_err().to_string();
    assert!(err_msg.contains("optimistic concurrency conflict"));

    // 4. Valid update with matching version (1) increments to version 2
    let mut updated_state = loaded.clone();
    updated_state.cookies.push(Cookie {
        name: "device_id".into(),
        value: "dev456".into(),
        domain: ".github.com".into(),
        path: "/".into(),
        expires: None,
        http_only: None,
        secure: Some(true),
        same_site: None,
        extra: HashMap::new(),
    });

    jars.save_jar("github.com", &updated_state)
        .expect("save with correct version must succeed");

    let loaded_v2 = jars.load_jar("github.com").unwrap();
    assert_eq!(loaded_v2.jar_version, 2);
    assert_eq!(loaded_v2.cookies.len(), 2);

    // 5. Hydration across multiple domain jars
    let google_state = StorageState {
        jar_version: 0,
        cookies: vec![Cookie {
            name: "SID".into(),
            value: "googlesid".into(),
            domain: ".google.com".into(),
            path: "/".into(),
            expires: None,
            http_only: Some(true),
            secure: Some(true),
            same_site: None,
            extra: HashMap::new(),
        }],
        origins: vec![],
    };
    jars.save_jar("google.com", &google_state)
        .expect("save google jar");

    let hydrated = jars.hydrate_jars(&["github.com".into(), "google.com".into()]);
    assert_eq!(hydrated.cookies.len(), 3);
    assert!(hydrated.cookies.iter().any(|c| c.name == "user_session"));
    assert!(hydrated.cookies.iter().any(|c| c.name == "SID"));

    // 6. Dump updated cookies back into domain jars
    let updated_cookies = vec![
        Cookie {
            name: "user_session".into(),
            value: "refreshed_token".into(),
            domain: ".github.com".into(),
            path: "/".into(),
            expires: None,
            http_only: Some(true),
            secure: Some(true),
            same_site: None,
            extra: HashMap::new(),
        },
        Cookie {
            name: "new_google_cookie".into(),
            value: "val999".into(),
            domain: "google.com".into(),
            path: "/".into(),
            expires: None,
            http_only: None,
            secure: None,
            same_site: None,
            extra: HashMap::new(),
        },
    ];

    jars.dump_cookies_to_jars(
        &updated_cookies,
        &["github.com".into(), "google.com".into()],
    )
    .expect("dump cookies to jars must succeed");

    let post_dump_github = jars.load_jar("github.com").unwrap();
    assert_eq!(post_dump_github.jar_version, 3);
    let session_cookie = post_dump_github
        .cookies
        .iter()
        .find(|c| c.name == "user_session")
        .unwrap();
    assert_eq!(session_cookie.value, "refreshed_token");

    let post_dump_google = jars.load_jar("google.com").unwrap();
    assert_eq!(post_dump_google.jar_version, 2);
    assert!(
        post_dump_google
            .cookies
            .iter()
            .any(|c| c.name == "new_google_cookie")
    );
}

#[test]
fn test_http_423_locked_response() {
    let holder = LockHolderInfo::new(Some(0), Some("browse".into()), Some("agent-x".into()));
    let err = ProfileLockError::Locked {
        profile: "locked-profile".into(),
        holder: Some(holder),
    };

    let resp = err.into_response();
    assert_eq!(resp.status(), StatusCode::LOCKED);
    assert_eq!(resp.status().as_u16(), 423);

    let timeout_err = ProfileLockError::Timeout {
        profile: "timeout-profile".into(),
        timeout_ms: 1000,
        holder: None,
    };
    let resp_timeout = timeout_err.into_response();
    assert_eq!(resp_timeout.status(), StatusCode::LOCKED);
    assert_eq!(resp_timeout.status().as_u16(), 423);
}

#[test]
fn test_locks_dir_outside_profile_dir_with_0700_permissions() {
    let tmp_dir = TempDir::new();
    let base_dir = tmp_dir.path().join("profiles");
    let locks_dir = tmp_dir.path().join("locks");
    let broker = ProfileBroker::new_with_locks_dir(base_dir.clone(), locks_dir.clone());

    let lease = broker
        .acquire("my-profile", 0)
        .expect("acquire should succeed");

    // Lock file lives under locks_dir
    let expected_lock = locks_dir.join("my-profile.lock");
    assert_eq!(lease.lock_path(), expected_lock);
    assert!(expected_lock.exists());

    // Profile dir does not have .reach.lock
    assert!(!lease.profile_dir().join(".reach.lock").exists());

    // On Unix, locks_dir has 0700 permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::metadata(&locks_dir).unwrap().permissions();
        assert_eq!(perm.mode() & 0o777, 0o700);
    }
}

#[test]
fn test_singleton_lock_detection_with_active_pid() {
    let tmp_dir = TempDir::new();
    let broker = ProfileBroker::new(tmp_dir.path().to_path_buf());
    let profile_name = "profile-chrome-active";
    let profile_dir = broker.profile_dir_for(profile_name);
    std::fs::create_dir_all(&profile_dir).unwrap();

    let current_pid = std::process::id();
    let symlink_path = profile_dir.join("SingletonLock");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(format!("myhost-{current_pid}"), &symlink_path).unwrap();
    }

    #[cfg(unix)]
    {
        assert!(broker.is_locked(profile_name));
        let holder = broker
            .holder_info(profile_name)
            .expect("holder should be found");
        assert_eq!(holder.pid, current_pid);
        assert_eq!(holder.task.as_deref(), Some("chrome"));
        assert_eq!(holder.owner.as_deref(), Some("chrome"));

        // Competing acquire must return Locked with holder indicating Chrome
        let res = broker.acquire(profile_name, 0);
        match res {
            Err(ProfileLockError::Locked { profile, holder }) => {
                assert_eq!(profile, profile_name);
                let h = holder.expect("holder must be present");
                assert_eq!(h.pid, current_pid);
                assert_eq!(h.task.as_deref(), Some("chrome"));
            }
            other => panic!("expected ProfileLockError::Locked, got {other:?}"),
        }

        // Clean up symlink
        let _ = std::fs::remove_file(symlink_path);
    }
}

#[test]
fn test_singleton_lock_clean_handling_with_dead_pid() {
    let tmp_dir = TempDir::new();
    let broker = ProfileBroker::new(tmp_dir.path().to_path_buf());
    let profile_name = "profile-chrome-dead";
    let profile_dir = broker.profile_dir_for(profile_name);
    std::fs::create_dir_all(&profile_dir).unwrap();

    let dead_pid = 99_999_999;
    let symlink_path = profile_dir.join("SingletonLock");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(format!("myhost-{dead_pid}"), &symlink_path).unwrap();
        assert!(symlink_path.symlink_metadata().is_ok());

        // is_locked should detect dead PID, remove stale symlink, and return false
        assert!(!broker.is_locked(profile_name));
        assert!(symlink_path.symlink_metadata().is_err());

        // Re-create dead symlink to test acquire recovery
        std::os::unix::fs::symlink(format!("crashhost-{dead_pid}"), &symlink_path).unwrap();
        let lease = broker
            .acquire(profile_name, 0)
            .expect("acquire must succeed by recovering from stale SingletonLock");
        assert_eq!(lease.profile(), profile_name);
        assert!(symlink_path.symlink_metadata().is_err());
    }
}

#[test]
fn test_stale_holder_pid_reclaim() {
    let tmp_dir = TempDir::new();
    let broker = ProfileBroker::new(tmp_dir.path().to_path_buf());
    let profile_name = "profile-stale-holder";
    let lock_path = broker.lock_path_for(profile_name);

    // Ensure parent dir exists
    std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();

    let dead_pid = 99_999_999;
    let stale_holder = LockHolderInfo {
        pid: dead_pid,
        screen: Some(0),
        task: Some("crashed-task".into()),
        owner: Some("dead-agent".into()),
        acquired_at: chrono::Utc::now(),
        lease_id: "stale-lease-123".into(),
    };
    std::fs::write(&lock_path, serde_json::to_string(&stale_holder).unwrap()).unwrap();

    // is_locked detects dead PID, cleans up stale file, and returns false
    assert!(!broker.is_locked(profile_name));
    assert_eq!(broker.holder_info(profile_name), None);

    // Re-write stale file and verify acquire reclaims it cleanly
    std::fs::write(&lock_path, serde_json::to_string(&stale_holder).unwrap()).unwrap();
    let lease = broker
        .acquire(profile_name, 0)
        .expect("acquire should succeed by reclaiming stale lock");
    assert_eq!(lease.profile(), profile_name);
    assert_eq!(lease.holder().pid, std::process::id());
}
