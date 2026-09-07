use clap::{ArgAction, Args};
use colored::Colorize;
use reach_cli::config::ReachConfig;
use reach_cli::docker::{
    DockerClient, LifecycleMode, ProfileMount, ResetManifest, Resolution, SandboxConfig,
    SandboxPorts, read_reset_manifest, reset_manifest_for, reset_manifest_path,
    validate_sandbox_config, write_reset_manifest,
};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args, Clone, Debug, Default)]
#[group(id = "lifecycle-mode", multiple = false)]
pub struct LifecycleArgs {
    /// Start without preserving any host-backed state (the default).
    #[arg(long, action = ArgAction::SetTrue)]
    pub clean: bool,

    /// Start with one-shot state supplied by an authorized hydration flow.
    ///
    /// Hydrated cookies and other credentials are intentionally not persisted
    /// in the lifecycle manifest.
    #[arg(long, action = ArgAction::SetTrue)]
    pub hydrated: bool,

    /// Preserve only explicitly configured host-backed mounts.
    #[arg(long, action = ArgAction::SetTrue)]
    pub persistent: bool,
}

impl LifecycleArgs {
    pub fn explicit_mode(&self) -> Option<LifecycleMode> {
        if self.clean {
            Some(LifecycleMode::Clean)
        } else if self.hydrated {
            Some(LifecycleMode::Hydrated)
        } else if self.persistent {
            Some(LifecycleMode::Persistent)
        } else {
            None
        }
    }
}

/// Resolve create's lifecycle choice. State is clean by default; a profile
/// mount is never silently downgraded to a clean lifecycle.
pub fn create_lifecycle_mode(
    lifecycle: &LifecycleArgs,
    has_profile: bool,
    has_workspace: bool,
) -> anyhow::Result<LifecycleMode> {
    let mode = lifecycle.explicit_mode().unwrap_or(LifecycleMode::Clean);
    if has_profile && mode != LifecycleMode::Persistent {
        anyhow::bail!("--persist-profile requires explicit --persistent");
    }
    if mode == LifecycleMode::Hydrated && has_workspace {
        anyhow::bail!(
            "--hydrated cannot be combined with --workspace; use --persistent for durable mounts"
        );
    }
    if mode == LifecycleMode::Persistent && !has_profile && !has_workspace {
        anyhow::bail!("--persistent requires --persist-profile or --workspace");
    }
    Ok(mode)
}

/// Stable manifest location for a sandbox. The configured workspace root is
/// used as a state root even when a sandbox mounts a custom workspace path, so
/// a clean recreation can still find its manifest after removing that mount.
pub fn lifecycle_manifest_path(cfg: &ReachConfig, name: &str) -> PathBuf {
    reset_manifest_path(&cfg.sandbox.resolved_workspace_dir(), name)
}

/// Set the manifest's mode without ever adding runtime credentials or
/// capability state to it.
pub fn lifecycle_manifest_for(config: &SandboxConfig, mode: LifecycleMode) -> ResetManifest {
    let mut manifest = reset_manifest_for(config);
    manifest.mode = mode;
    if !matches!(mode, LifecycleMode::Persistent) {
        manifest.workspace = None;
        manifest.profile_name = None;
        manifest.writable_workspace = false;
        manifest.restart_unless_stopped = false;
    }
    manifest
}

pub(crate) fn persist_lifecycle_manifest(
    cfg: &ReachConfig,
    config: &SandboxConfig,
    mode: LifecycleMode,
) -> anyhow::Result<PathBuf> {
    let path = lifecycle_manifest_path(cfg, &config.name);
    write_reset_manifest(&path, &lifecycle_manifest_for(config, mode))?;
    Ok(path)
}
pub(crate) fn lifecycle_manifest_candidates(
    cfg: &ReachConfig,
    config: &SandboxConfig,
) -> Vec<PathBuf> {
    let stable = lifecycle_manifest_path(cfg, &config.name);
    let mut paths = vec![stable.clone()];
    if let Some(workspace) = &config.workspace {
        let adjacent = reset_manifest_path(workspace, &config.name);
        if adjacent != stable {
            paths.push(adjacent);
        }
    }
    paths
}

pub(crate) fn read_lifecycle_manifest(
    cfg: &ReachConfig,
    config: &SandboxConfig,
) -> anyhow::Result<(PathBuf, ResetManifest)> {
    let existing: Vec<PathBuf> = lifecycle_manifest_candidates(cfg, config)
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    let path = match existing.as_slice() {
        [] => anyhow::bail!(
            "sandbox '{}' has no lifecycle manifest; refusing to inherit runtime state. \
             Recover with `reach create --name {} --clean` (or explicitly recreate with \
             `--persistent` after restoring its manifest)",
            config.name,
            config.name
        ),
        [path] => path,
        _ => anyhow::bail!(
            "sandbox '{}' has ambiguous lifecycle manifests at {}; remove the stale copy \
             before retrying",
            config.name,
            existing
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let manifest = read_reset_manifest(path)?;
    anyhow::ensure!(
        manifest.schema_version == reach_cli::docker::RESET_MANIFEST_SCHEMA_VERSION,
        "unsupported lifecycle manifest schema {} for sandbox '{}'",
        manifest.schema_version,
        config.name
    );
    anyhow::ensure!(
        manifest.name == config.name,
        "lifecycle manifest name '{}' does not match sandbox '{}'",
        manifest.name,
        config.name
    );
    match manifest.mode {
        LifecycleMode::Persistent => anyhow::ensure!(
            manifest.workspace.is_some() || manifest.profile_name.is_some(),
            "persistent lifecycle manifest for '{}' authorizes no persistent mount",
            config.name
        ),
        LifecycleMode::Clean | LifecycleMode::Hydrated => anyhow::ensure!(
            manifest.workspace.is_none() && manifest.profile_name.is_none(),
            "non-persistent lifecycle manifest for '{}' contains mount state",
            config.name
        ),
    }
    Ok((path.clone(), manifest))
}

#[derive(Args)]
pub struct CreateArgs {
    #[command(flatten)]
    pub lifecycle: LifecycleArgs,

    /// Name for the sandbox container
    #[arg(long, default_value = "reach")]
    pub name: String,

    /// Display resolution (WxH)
    #[arg(long, default_value = "1280x720")]
    pub resolution: String,

    /// Docker image to use
    #[arg(long)]
    pub image: Option<String>,

    /// VNC port
    #[arg(long)]
    pub vnc_port: Option<u16>,

    /// noVNC port
    #[arg(long)]
    pub novnc_port: Option<u16>,

    /// Health API port
    #[arg(long)]
    pub health_port: Option<u16>,

    /// Publish an additional port from the sandbox to the host.
    ///
    /// Format: `HOST:CONTAINER` or `PORT` (same on both sides). Repeat the
    /// flag to publish more than one. Example: `--extra-port 9222:9222`
    /// exposes Chrome's CDP debug port so a host process can drive a
    /// browser inside the sandbox.
    #[arg(long = "extra-port", value_name = "HOST:CONTAINER", value_parser = parse_port_pair)]
    pub extra_ports: Vec<(u16, u16)>,

    /// Skip waiting for health check
    #[arg(long)]
    pub no_wait: bool,

    /// Persist a Chrome profile across sandbox restarts.
    ///
    /// The named profile is stored on the host under
    /// `~/.local/share/reach/profiles/<name>` (overridable via the
    /// `sandbox.profile_dir` config key) and bind-mounted into the
    /// container at `/home/sandbox/.config/google-chrome-profiles/<name>`.
    /// Pass the same name to `page_text` / `auth_handoff` via
    /// `use_profile` to reuse the session.
    #[arg(long, value_name = "NAME")]
    pub persist_profile: Option<String>,

    /// Mount a host directory at /workspace (durable files). Without a value,
    /// uses `<workspace_dir>/<name>` (default ~/.local/share/reach/workspaces/<name>).
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    pub workspace: Option<String>,

    /// Hard memory cap, e.g. `2.5g` (default: sandbox.memory from config, else unlimited)
    #[arg(long, value_parser = parse_memory)]
    pub memory: Option<u64>,

    /// Do not set the `unless-stopped` restart policy.
    ///
    /// The restart policy is only applied when `--workspace` is set (an
    /// ephemeral sandbox without a workspace has nothing durable to survive
    /// a restart into, so it defaults off). Pass this flag to opt out even
    /// when `--workspace` is set.
    #[arg(long)]
    pub no_restart: bool,

    /// Require this password for VNC access (also prompted by noVNC).
    ///
    /// Default: `sandbox.vnc_password` from config, else no password.
    /// The password reaches the container via an env var; the container
    /// is the trust boundary. Never printed by `reach create` or
    /// `reach list`.
    #[arg(long, value_name = "PW")]
    pub vnc_password: Option<String>,

    /// Number of virtual screens / displays (default: 1)
    #[arg(long, default_value = "1")]
    pub screens: u32,

    /// Allow arbitrary shell command execution inside the container via the `exec` tool.
    /// Default: false (refuses `exec` calls unless explicitly enabled).
    #[arg(long)]
    pub allow_exec: bool,

    /// Make the /workspace bind mount writable inside the container.
    /// Default: false (mounts /workspace as read-only).
    #[arg(long)]
    pub writable_workspace: bool,
}

/// Parse a `HOST:CONTAINER` port pair, or a single `PORT` shorthand for
/// `PORT:PORT`. Returns an error for malformed input or out-of-range numbers.
fn parse_port_pair(s: &str) -> Result<(u16, u16), String> {
    if let Some((h, c)) = s.split_once(':') {
        let host: u16 = h.parse().map_err(|_| format!("invalid host port {h:?}"))?;
        let container: u16 = c
            .parse()
            .map_err(|_| format!("invalid container port {c:?}"))?;
        Ok((host, container))
    } else {
        let p: u16 = s.parse().map_err(|_| format!("invalid port {s:?}"))?;
        Ok((p, p))
    }
}

/// Parse `512m`, `2g`, `2.5G`, or raw bytes.
pub fn parse_memory(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1024f64),
        Some('m' | 'M') => (&s[..s.len() - 1], 1024f64 * 1024.0),
        Some('g' | 'G') => (&s[..s.len() - 1], 1024f64 * 1024.0 * 1024.0),
        _ => (s, 1.0),
    };
    let n: f64 = num
        .parse()
        .map_err(|_| format!("invalid memory size {s:?}"))?;
    if !n.is_finite() || n < 0.0 {
        return Err(format!("invalid memory size {s:?}"));
    }
    Ok((n * mult) as u64)
}

pub async fn run(args: CreateArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load();
    let resolution = Resolution::parse(&args.resolution)?;

    let profile = args.persist_profile.as_ref().map(|name| {
        let host_path = ProfileMount::host_path_for(&cfg.sandbox.resolved_profile_dir(), name);
        ProfileMount {
            name: name.clone(),
            host_path,
            container_path: ProfileMount::container_path_for(name),
        }
    });

    let workspace = args.workspace.as_ref().map(|w| {
        if w.is_empty() {
            cfg.sandbox.resolved_workspace_dir().join(&args.name)
        } else {
            std::path::PathBuf::from(w)
        }
    });

    let memory = args.memory.or(cfg.sandbox.memory);
    // Empty string means "no password" (same as unset) — never report auth
    // as on when x11vnc would actually run with `-nopw`.
    let vnc_password = args
        .vnc_password
        .clone()
        .or(cfg.sandbox.vnc_password.clone())
        .filter(|s| !s.is_empty());

    let config = SandboxConfig {
        name: args.name.clone(),
        image: args.image.unwrap_or(cfg.sandbox.image.clone()),
        resolution,
        shm_size: cfg.sandbox.shm_size,
        ports: SandboxPorts {
            vnc: args.vnc_port.unwrap_or(cfg.sandbox.vnc_port),
            novnc: args.novnc_port.unwrap_or(cfg.sandbox.novnc_port),
            health: args.health_port.unwrap_or(cfg.sandbox.health_port),
            extra: args.extra_ports.clone(),
        },
        screens: args.screens.max(1),
        profile,
        workspace: workspace.clone(),
        memory,
        restart_unless_stopped: workspace.is_some() && !args.no_restart,
        vnc_password: vnc_password.clone(),
        allow_exec: args.allow_exec,
        writable_workspace: args.writable_workspace,
    };

    let mode = create_lifecycle_mode(
        &args.lifecycle,
        config.profile.is_some(),
        config.workspace.is_some(),
    )?;
    validate_sandbox_config(&config)?;
    persist_lifecycle_manifest(&cfg, &config, mode)?;

    let docker = DockerClient::new(cfg.docker.socket_path())?;
    let sandbox = docker.create(config).await?;

    println!();
    println!("  {}", "reach create".bold());
    println!("  {}", "\u{2500}".repeat(28).dimmed());
    println!(
        "  {} {}  {}",
        "\u{2713}".green(),
        "Container ".dimmed(),
        &sandbox.container_id[..12]
    );
    println!(
        "  {} {}  {}",
        "\u{2713}".green(),
        "Image     ".dimmed(),
        sandbox.image
    );
    println!(
        "  {} {}  {}",
        "\u{2713}".green(),
        "Resolution".dimmed(),
        args.resolution
    );
    println!(
        "  {} {}  {}",
        "\u{2713}".green(),
        "Screens   ".dimmed(),
        sandbox.ports.screens
    );

    if let Some(name) = &args.persist_profile {
        let host = ProfileMount::host_path_for(&cfg.sandbox.resolved_profile_dir(), name);
        println!(
            "  {} {}  {} {}",
            "\u{2713}".green(),
            "Profile   ".dimmed(),
            name,
            format!("({})", host.display()).dimmed()
        );
    }

    if let Some(ws) = &workspace {
        let mode = if args.writable_workspace {
            "writable"
        } else {
            "read-only"
        };
        println!(
            "  {} {}  {} {}",
            "\u{2713}".green(),
            "Workspace ".dimmed(),
            ws.display(),
            format!("({mode})").dimmed()
        );
    }

    if args.allow_exec {
        println!(
            "  {} {}  enabled",
            "\u{2713}".green(),
            "Allow Exec".dimmed(),
        );
    }

    if let Some(mem) = memory {
        let gb = mem as f64 / (1024.0 * 1024.0 * 1024.0);
        let mb = mem as f64 / (1024.0 * 1024.0);
        let mem_str = if gb >= 1.0 {
            format!("{:.1}G", gb)
        } else {
            format!("{:.0}M", mb)
        };
        println!(
            "  {} {}  {}",
            "\u{2713}".green(),
            "Memory    ".dimmed(),
            mem_str
        );
    }

    if vnc_password.is_some() {
        println!(
            "  {} {}  password set",
            "\u{2713}".green(),
            "VNC       ".dimmed(),
        );
    }

    if !args.no_wait {
        print!("  \u{2819} {}", "Waiting for health...".dimmed());
        docker
            .wait_healthy(&args.name, Duration::from_secs(30))
            .await?;
        print!("\r");
        println!("  {} Healthy", "\u{2713}".green());
    }

    println!();
    println!(
        "    {}  reach serve --sandbox {}",
        "API:".bold(),
        sandbox.name.cyan()
    );
    println!(
        "    {}  reach vnc {} (after a worker leases the screen)",
        "Viewer:".bold(),
        sandbox.name.cyan()
    );
    if let Some(p) = sandbox.ports.health {
        println!(
            "    {}  {}",
            "Health:".bold(),
            format!("http://localhost:{}/health", p).cyan()
        );
    }
    for (host_port, container_port) in &sandbox.ports.extra {
        println!(
            "    {}    localhost:{} -> {}/tcp",
            "Extra:".bold(),
            host_port.to_string().cyan(),
            container_port.to_string().cyan()
        );
    }

    println!();
    println!(
        "  Sandbox {} ready.",
        format!("\"{}\"", sandbox.name).green().bold()
    );
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_memory;

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_memory("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("2.5G").unwrap(), 2_684_354_560);
        assert_eq!(parse_memory("1024").unwrap(), 1024);
        assert!(parse_memory("lots").is_err());
        assert!(parse_memory("-5g").is_err());
        assert!(parse_memory("nan").is_err());
        assert!(parse_memory("inf").is_err());
    }
}
