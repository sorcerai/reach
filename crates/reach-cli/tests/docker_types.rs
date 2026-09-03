//! Unit tests for Docker abstraction types — sandbox config, resolution
//! parsing, labels, status mapping.

use reach_cli::docker::*;
use std::path::PathBuf;

// ═══════════════════════════════════════════════════════════
// Resolution parsing
// ═══════════════════════════════════════════════════════════

#[test]
fn resolution_parses_standard_formats() {
    let r = Resolution::parse("1280x720").unwrap();
    assert_eq!(r.width, 1280);
    assert_eq!(r.height, 720);

    let r = Resolution::parse("1920x1080").unwrap();
    assert_eq!(r.width, 1920);
    assert_eq!(r.height, 1080);
}

#[test]
fn resolution_display_roundtrips() {
    let r = Resolution::parse("1280x720").unwrap();
    assert_eq!(r.to_string(), "1280x720");
}

#[test]
fn resolution_rejects_garbage() {
    assert!(Resolution::parse("not-a-resolution").is_err());
    assert!(Resolution::parse("1280").is_err());
    assert!(Resolution::parse("x720").is_err());
    assert!(Resolution::parse("").is_err());
}

#[test]
fn resolution_rejects_non_numeric() {
    assert!(Resolution::parse("widexhigh").is_err());
}

// ═══════════════════════════════════════════════════════════
// Sandbox config defaults
// ═══════════════════════════════════════════════════════════

#[test]
fn sandbox_config_default_image_is_reach_latest() {
    let config = SandboxConfig::default();
    assert_eq!(config.image, "reach:latest");
}

#[test]
fn sandbox_config_default_resolution_is_720p() {
    let config = SandboxConfig::default();
    assert_eq!(config.resolution.width, 1280);
    assert_eq!(config.resolution.height, 720);
}

#[test]
fn default_sandbox_config_has_no_workspace_and_512mb_shm() {
    let c = SandboxConfig::default();
    assert_eq!(c.shm_size, 512 * 1024 * 1024);
    assert!(c.workspace.is_none());
    assert_eq!(c.memory, None);
    assert!(c.restart_unless_stopped);
}

#[test]
fn build_mounts_includes_workspace_and_profile() {
    let c = SandboxConfig {
        workspace: Some(PathBuf::from("/tmp/ws")),
        profile: Some(ProfileMount {
            name: "default".into(),
            host_path: PathBuf::from("/tmp/prof"),
            container_path: ProfileMount::container_path_for("default"),
        }),
        ..SandboxConfig::default()
    };
    let mounts = build_mounts(&c);
    let targets: Vec<String> = mounts.iter().map(|m| m.target.clone().unwrap()).collect();
    assert!(targets.contains(&WORKSPACE_CONTAINER_PATH.to_string()));
    assert!(targets.contains(&"/home/sandbox/.config/google-chrome-profiles/default".to_string()));
}

#[test]
fn labels_include_workspace_when_set() {
    let c = SandboxConfig {
        workspace: Some(PathBuf::from("/tmp/ws")),
        ..SandboxConfig::default()
    };
    let l = Labels::for_sandbox(&c);
    assert_eq!(
        l.get(Labels::WORKSPACE).map(String::as_str),
        Some("/tmp/ws")
    );
}

#[test]
fn sandbox_config_default_ports() {
    let config = SandboxConfig::default();
    assert_eq!(config.ports.vnc, 5900);
    assert_eq!(config.ports.novnc, 6080);
    assert_eq!(config.ports.health, 8400);
}

// ═══════════════════════════════════════════════════════════
// Container labels
// ═══════════════════════════════════════════════════════════

#[test]
fn labels_for_sandbox_includes_all_required_keys() {
    let config = SandboxConfig::default();
    let labels = Labels::for_sandbox(&config);

    assert_eq!(labels.get(Labels::MANAGED), Some(&"true".to_string()));
    assert_eq!(labels.get(Labels::NAME), Some(&config.name));
    assert!(labels.contains_key(Labels::CREATED));
    assert!(labels.contains_key(Labels::RESOLUTION));
}

#[test]
fn labels_filter_targets_managed_containers() {
    let filter = Labels::filter();
    let label_filters = filter.get("label").unwrap();
    assert!(
        label_filters
            .iter()
            .any(|f| f.contains("reach.sandbox=true"))
    );
}

// ═══════════════════════════════════════════════════════════
// Sandbox status mapping
// ═══════════════════════════════════════════════════════════

#[test]
fn sandbox_status_from_docker_state() {
    assert_eq!(SandboxStatus::from("running"), SandboxStatus::Running);
    assert_eq!(SandboxStatus::from("created"), SandboxStatus::Starting);
    assert_eq!(SandboxStatus::from("restarting"), SandboxStatus::Starting);
    assert_eq!(SandboxStatus::from("exited"), SandboxStatus::Stopped);
    assert_eq!(SandboxStatus::from("dead"), SandboxStatus::Stopped);
    assert_eq!(SandboxStatus::from("paused"), SandboxStatus::Unknown);
    assert_eq!(SandboxStatus::from("banana"), SandboxStatus::Unknown);
}

// ═══════════════════════════════════════════════════════════
// Sandbox serialization
// ═══════════════════════════════════════════════════════════

#[test]
fn sandbox_serializes_to_json() {
    let sandbox = Sandbox {
        name: "test".into(),
        container_id: "abc123".into(),
        status: SandboxStatus::Running,
        image: "reach:latest".into(),
        ports: SandboxPortMapping {
            vnc: Some(5900),
            novnc: Some(6080),
            health: Some(8400),
            extra: Vec::new(),
        },
        created_at: "2026-04-02T00:00:00Z".into(),
    };

    let json = serde_json::to_value(&sandbox).unwrap();
    assert_eq!(json["name"], "test");
    assert_eq!(json["status"], "running");
    assert_eq!(json["ports"]["vnc"], 5900);
}

// ═══════════════════════════════════════════════════════════
// ExecOutput
// ═══════════════════════════════════════════════════════════

#[test]
fn exec_output_serializes() {
    let output = ExecOutput {
        exit_code: 0,
        stdout: "hello\n".into(),
        stderr: "".into(),
    };
    let json = serde_json::to_value(&output).unwrap();
    assert_eq!(json["exit_code"], 0);
    assert_eq!(json["stdout"], "hello\n");
}

// ═══════════════════════════════════════════════════════════
// Profile mounts
// ═══════════════════════════════════════════════════════════

#[test]
fn profile_mount_container_path_uses_stable_root() {
    assert_eq!(
        ProfileMount::container_path_for("threads"),
        "/home/sandbox/.config/google-chrome-profiles/threads"
    );
}

#[test]
fn profile_mount_host_path_joins_under_base() {
    let base = std::path::Path::new("/var/lib/reach/profiles");
    let host = ProfileMount::host_path_for(base, "threads");
    assert_eq!(
        host,
        std::path::PathBuf::from("/var/lib/reach/profiles/threads")
    );
}

#[test]
fn labels_for_sandbox_includes_profile_label_when_set() {
    let config = SandboxConfig {
        profile: Some(ProfileMount {
            name: "threads".into(),
            host_path: std::path::PathBuf::from("/tmp/x"),
            container_path: ProfileMount::container_path_for("threads"),
        }),
        ..SandboxConfig::default()
    };
    let labels = Labels::for_sandbox(&config);
    assert_eq!(labels.get(Labels::PROFILE), Some(&"threads".to_string()));
}

#[test]
fn labels_for_sandbox_omits_profile_label_when_unset() {
    let config = SandboxConfig::default();
    let labels = Labels::for_sandbox(&config);
    assert!(!labels.contains_key(Labels::PROFILE));
}

// ═══════════════════════════════════════════════════════════
// config_from_inspect (recreate)
// ═══════════════════════════════════════════════════════════

fn fake_inspect(
    labels: &[(&str, &str)],
    image: &str,
    port_bindings: &[(&str, &str)],
    memory: Option<i64>,
    shm_size: Option<i64>,
    restart_unless_stopped: bool,
) -> bollard::models::ContainerInspectResponse {
    use bollard::models::{
        ContainerConfig, ContainerInspectResponse, HostConfig, PortBinding, RestartPolicy,
        RestartPolicyNameEnum,
    };
    use std::collections::HashMap;

    let mut label_map = HashMap::new();
    for (k, v) in labels {
        label_map.insert(k.to_string(), v.to_string());
    }

    let mut pb_map: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for (container_port, host_port) in port_bindings {
        pb_map.insert(
            container_port.to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".into()),
                host_port: Some(host_port.to_string()),
            }]),
        );
    }

    ContainerInspectResponse {
        config: Some(ContainerConfig {
            image: Some(image.to_string()),
            labels: Some(label_map),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            port_bindings: Some(pb_map),
            memory,
            shm_size,
            restart_policy: restart_unless_stopped.then_some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn config_from_inspect_roundtrips_basic_fields() {
    let resp = fake_inspect(
        &[
            (Labels::NAME, "reach-e2e"),
            (Labels::RESOLUTION, "1280x720"),
            (Labels::WORKSPACE, "/tmp/reach-e2e-ws"),
        ],
        "reach:latest",
        &[
            ("5900/tcp", "15900"),
            ("6080/tcp", "16080"),
            ("8400/tcp", "18400"),
            ("9222/tcp", "19222"),
        ],
        Some(2560 * 1024 * 1024),
        Some(512 * 1024 * 1024),
        true,
    );

    let fallback = std::path::Path::new("/tmp/unused-profiles");
    let config = config_from_inspect(&resp, fallback).unwrap();

    assert_eq!(config.name, "reach-e2e");
    assert_eq!(config.image, "reach:latest");
    assert_eq!(config.resolution.to_string(), "1280x720");
    assert_eq!(config.ports.vnc, 15900);
    assert_eq!(config.ports.novnc, 16080);
    assert_eq!(config.ports.health, 18400);
    assert_eq!(config.ports.extra, vec![(19222, 9222)]);
    assert_eq!(config.memory, Some(2560 * 1024 * 1024));
    assert_eq!(config.shm_size, 512 * 1024 * 1024);
    assert!(config.restart_unless_stopped);
    assert_eq!(config.workspace, Some(PathBuf::from("/tmp/reach-e2e-ws")));
    assert!(config.profile.is_none());
}

#[test]
fn config_from_inspect_falls_back_to_profile_dir_when_host_missing() {
    let resp = fake_inspect(
        &[
            (Labels::NAME, "reach-e2e"),
            (Labels::RESOLUTION, "1280x720"),
            (Labels::PROFILE, "default"),
        ],
        "reach:latest",
        &[],
        None,
        None,
        false,
    );

    let fallback = std::path::Path::new("/var/lib/reach/profiles");
    let config = config_from_inspect(&resp, fallback).unwrap();

    let profile = config.profile.expect("profile should round-trip");
    assert_eq!(profile.name, "default");
    assert_eq!(
        profile.host_path,
        PathBuf::from("/var/lib/reach/profiles/default")
    );
    assert!(!config.restart_unless_stopped);
    assert_eq!(config.memory, None);
}

#[test]
fn config_from_inspect_prefers_profile_host_label() {
    let resp = fake_inspect(
        &[
            (Labels::NAME, "reach-e2e"),
            (Labels::RESOLUTION, "1280x720"),
            (Labels::PROFILE, "default"),
            (Labels::PROFILE_HOST, "/custom/profiles/default"),
        ],
        "reach:latest",
        &[],
        None,
        None,
        false,
    );

    let fallback = std::path::Path::new("/var/lib/reach/profiles");
    let config = config_from_inspect(&resp, fallback).unwrap();

    let profile = config.profile.expect("profile should round-trip");
    assert_eq!(profile.host_path, PathBuf::from("/custom/profiles/default"));
}

#[test]
fn novnc_url_uses_localhost_pattern() {
    assert_eq!(
        novnc_url("localhost", 6080),
        "http://localhost:6080/vnc.html?autoconnect=1&resize=remote"
    );
}
