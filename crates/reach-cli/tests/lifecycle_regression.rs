//! Regression coverage for lifecycle and screen-reset decisions that do not
//! require a live Docker daemon.

use reach_cli::docker::{
    LifecycleMode, ProfileMount, SandboxConfig, lifecycle_mode, read_reset_manifest,
    reset_manifest_for, reset_manifest_path, screen_cdp_port, screen_display,
    validate_sandbox_config, write_reset_manifest,
};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[test]
fn lifecycle_modes_keep_hydration_ephemeral_and_profiles_persistent() {
    assert_eq!(lifecycle_mode(None, false, false), LifecycleMode::Clean);
    assert_eq!(lifecycle_mode(None, true, false), LifecycleMode::Hydrated);
    assert_eq!(lifecycle_mode(None, false, true), LifecycleMode::Persistent);
    let profile = ProfileMount {
        name: "personal".into(),
        host_path: PathBuf::from("/tmp/profile"),
        container_path: ProfileMount::container_path_for("personal"),
    };
    assert_eq!(
        lifecycle_mode(Some(&profile), true, false),
        LifecycleMode::Persistent
    );
}

#[test]
fn code_capable_sandbox_rejects_personal_profile_but_allows_workspace() {
    let profile = ProfileMount {
        name: "personal".into(),
        host_path: PathBuf::from("/tmp/profile"),
        container_path: ProfileMount::container_path_for("personal"),
    };
    let rejected = SandboxConfig {
        allow_exec: true,
        profile: Some(profile),
        ..SandboxConfig::default()
    };
    assert!(validate_sandbox_config(&rejected).is_err());

    let workspace = SandboxConfig {
        allow_exec: true,
        workspace: Some(PathBuf::from("/tmp/workspace")),
        ..SandboxConfig::default()
    };
    assert!(validate_sandbox_config(&workspace).is_ok());
}

#[test]
fn reset_manifest_omits_credentials_and_runtime_capabilities() {
    let config = SandboxConfig {
        name: "clean-clone".into(),
        image: "reach:test".into(),
        workspace: Some(PathBuf::from("/tmp/workspace")),
        vnc_password: Some("do-not-persist".into()),
        allow_exec: true,
        ..SandboxConfig::default()
    };
    let manifest = reset_manifest_for(&config);
    let json = serde_json::to_string(&manifest).unwrap();
    assert_eq!(manifest.mode, LifecycleMode::Persistent);
    assert!(!json.contains("do-not-persist"));
    assert!(!json.contains("allow_exec"));
    assert!(!json.contains("capability"));
    assert!(!json.contains("ref"));
}

#[test]
fn reset_manifest_is_private_and_atomically_replaceable() {
    let root = tempfile_path();
    let path = reset_manifest_path(&root, "screen-0");
    let config = SandboxConfig {
        name: "screen-0".into(),
        workspace: Some(root.join("workspace")),
        ..SandboxConfig::default()
    };
    write_reset_manifest(&path, &reset_manifest_for(&config)).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let loaded = read_reset_manifest(&path).unwrap();
    assert_eq!(loaded, reset_manifest_for(&config));
    let bytes = std::fs::read(&path).unwrap();
    assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn screen_identity_helpers_are_bounded_and_exact() {
    assert_eq!(screen_display(0), ":99");
    assert_eq!(screen_display(3), ":102");
    assert_eq!(screen_cdp_port(0).unwrap(), 9222);
    assert_eq!(screen_cdp_port(3).unwrap(), 9225);
    assert!(screen_cdp_port(u32::MAX).is_err());
}

fn tempfile_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "reach-lifecycle-regression-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}
