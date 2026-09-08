use clap::Args;
use colored::Colorize;
use reach_cli::config::ReachConfig;
use reach_cli::docker::{
    LifecycleMode, ProfileMount, ResetManifest, SandboxConfig, validate_sandbox_config,
};
use reach_cli::runtime::RuntimeClient;
use std::time::Duration;

use super::create::{
    LifecycleArgs, lifecycle_manifest_path, persist_lifecycle_manifest, read_lifecycle_manifest,
};

#[derive(Args)]
pub struct RecreateArgs {
    /// Sandbox name or container ID
    pub target: String,

    #[command(flatten)]
    pub lifecycle: LifecycleArgs,

    /// Replace the image (e.g. after `make lab-load`)
    #[arg(long)]
    pub image: Option<String>,

    /// Explicitly enable a fresh code-capable recreation; never inherited.
    #[arg(long)]
    pub allow_exec: bool,
}

fn config_for_lifecycle(
    allow_exec: bool,
    inspected: &SandboxConfig,
    manifest: &ResetManifest,
    mode: LifecycleMode,
    image: Option<String>,
) -> anyhow::Result<SandboxConfig> {
    if mode == LifecycleMode::Persistent {
        anyhow::ensure!(
            manifest.mode == LifecycleMode::Persistent,
            "manifest for '{}' does not authorize persistent recreation; pass --clean or \
             restore a persistent manifest",
            manifest.name
        );
    }

    let mut config = inspected.clone();
    config.image = image.unwrap_or_else(|| manifest.image.clone());
    config.vnc_password = None;
    config.allow_exec = allow_exec;
    config.profile = None;
    config.workspace = None;
    config.writable_workspace = false;
    config.restart_unless_stopped = false;

    if mode == LifecycleMode::Persistent {
        if let Some(workspace) = &manifest.workspace {
            config.workspace = Some(workspace.clone());
            config.writable_workspace = manifest.writable_workspace;
            config.restart_unless_stopped = manifest.restart_unless_stopped;
        }
        if let Some(profile_name) = &manifest.profile_name {
            let profile = inspected
                .profile
                .as_ref()
                .filter(|profile| profile.name == profile_name.as_str())
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "manifest authorizes profile '{}', but the current sandbox has no \
                         matching profile mount",
                        profile_name
                    )
                })?;
            config.profile = Some(ProfileMount {
                name: profile.name,
                host_path: profile.host_path,
                container_path: ProfileMount::container_path_for(profile_name),
            });
        }
    }

    // Hydration is supplied by the authorized cookie/profile mechanism at
    // request time; no hydrated state is copied into this config.
    Ok(config)
}

pub async fn run(args: RecreateArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load()?;
    let runtime = RuntimeClient::from_config(&cfg)?;
    let sandbox = runtime.find(&args.target).await?;
    let inspected = runtime.inspect_config(&sandbox.container_id).await?;
    let (manifest_path, manifest) = read_lifecycle_manifest(&cfg, &inspected)?;
    let mode = args
        .lifecycle
        .explicit_mode()
        .unwrap_or(LifecycleMode::Clean);
    let config = config_for_lifecycle(args.allow_exec, &inspected, &manifest, mode, args.image)?;
    validate_sandbox_config(&config)?;

    // Install the scrubbed manifest before destruction. If recreation fails,
    // a later attempt cannot fall back to the old password/capability state.
    persist_lifecycle_manifest(&cfg, &config, mode)?;
    runtime.destroy(&sandbox.container_id).await?;
    let new_sandbox = runtime.create(config).await?;
    runtime
        .wait_healthy(&new_sandbox.container_id, Duration::from_secs(45))
        .await?;

    // A custom/legacy adjacent manifest is no longer authoritative after a
    // successful recreation through the stable state root.
    if manifest_path != lifecycle_manifest_path(&cfg, &new_sandbox.name) {
        let _ = std::fs::remove_file(manifest_path);
    }

    println!(
        "{} recreated {} ({})",
        "\u{2713}".green(),
        new_sandbox.name,
        &new_sandbox.container_id[..12]
    );
    Ok(())
}
