use anyhow::{Context, Result, bail};
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{
    ContainerInspectResponse, HostConfig, Mount, MountTypeEnum, PortBinding, RestartPolicyNameEnum,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

// ═══════════════════════════════════════════════════════════
// Sandbox configuration
// ═══════════════════════════════════════════════════════════

/// Path inside the container where the durable workspace mount lands.
pub const WORKSPACE_CONTAINER_PATH: &str = "/workspace";

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub name: String,
    pub image: String,
    pub resolution: Resolution,
    pub shm_size: u64,
    pub ports: SandboxPorts,
    pub screens: u32,
    /// Optional persistent Chrome profile mount.
    pub profile: Option<ProfileMount>,
    /// Host directory bind-mounted at `/workspace` (durable files).
    pub workspace: Option<PathBuf>,
    /// Hard memory cap in bytes.
    pub memory: Option<u64>,
    /// Docker restart policy `unless-stopped` (always-on Agent Computer).
    pub restart_unless_stopped: bool,
    /// Optional VNC password. When set, `x11vnc` requires it and noVNC
    /// prompts for it in the browser. `None` disables VNC auth entirely.
    ///
    /// Never logged, never put in a container label — round-tripped for
    /// `recreate` via the container's `VNC_PASSWORD` env var instead.
    pub vnc_password: Option<String>,
}

/// Bind mount that backs a persistent Chrome profile.
///
/// `host_path` is created on the host (if missing) and mounted into the
/// container at `container_path`. `name` is propagated as the
/// `reach.profile` label so that `reach list` and downstream tools can
/// discover the profile attached to a sandbox.
#[derive(Debug, Clone)]
pub struct ProfileMount {
    pub name: String,
    pub host_path: PathBuf,
    pub container_path: String,
}

impl ProfileMount {
    /// Container path used for a profile of the given `name`.
    ///
    /// All persistent profiles live under
    /// `/home/sandbox/.config/google-chrome-profiles/<name>` in the
    /// container so the path is stable across sandboxes.
    pub fn container_path_for(name: &str) -> String {
        format!("/home/sandbox/.config/google-chrome-profiles/{name}")
    }

    /// Host path used for a profile of the given `name`, rooted at
    /// `base_dir` (typically `~/.local/share/reach/profiles`).
    pub fn host_path_for(base_dir: &std::path::Path, name: &str) -> PathBuf {
        base_dir.join(name)
    }
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('x').collect();
        anyhow::ensure!(parts.len() == 2, "resolution must be WxH (e.g., 1280x720)");
        Ok(Self {
            width: parts[0].parse().context("invalid width")?,
            height: parts[1].parse().context("invalid height")?,
        })
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

#[derive(Debug, Clone)]
pub struct SandboxPorts {
    pub vnc: u16,
    pub novnc: u16,
    pub health: u16,
    /// Additional host:container port pairs to publish, beyond the three
    /// built-in ports above. Used for ad-hoc workflows that need to expose
    /// extra services from inside the sandbox — e.g. forwarding Chrome's
    /// remote debugging port (9222) so a host process can drive an agent
    /// browser via CDP. Each tuple is (host_port, container_port).
    pub extra: Vec<(u16, u16)>,
}

impl Default for SandboxPorts {
    fn default() -> Self {
        Self {
            vnc: 5900,
            novnc: 6080,
            health: 8400,
            extra: Vec::new(),
        }
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            name: "reach".into(),
            image: "reach:latest".into(),
            resolution: Resolution {
                width: 1280,
                height: 720,
            },
            shm_size: 512 * 1024 * 1024,
            ports: SandboxPorts::default(),
            screens: 1,
            profile: None,
            workspace: None,
            memory: None,
            restart_unless_stopped: true,
            vnc_password: None,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Sandbox runtime state
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize)]
pub struct Sandbox {
    pub name: String,
    pub container_id: String,
    pub status: SandboxStatus,
    pub image: String,
    pub ports: SandboxPortMapping,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxStatus {
    Running,
    Starting,
    Stopped,
    Unhealthy,
    Unknown,
}

impl From<&str> for SandboxStatus {
    fn from(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "created" | "restarting" => Self::Starting,
            "exited" | "dead" => Self::Stopped,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SandboxPortMapping {
    pub vnc: Option<u16>,
    pub novnc: Option<u16>,
    pub health: Option<u16>,
    pub screens: u32,
    /// Extra (host_port, container_port) pairs published by the user via
    /// `--extra-port`. Empty when no extras were requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<(u16, u16)>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecOutput {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

// ═══════════════════════════════════════════════════════════
// Labels
// ═══════════════════════════════════════════════════════════

pub struct Labels;

impl Labels {
    pub const MANAGED: &str = "reach.sandbox";
    pub const NAME: &str = "reach.name";
    pub const CREATED: &str = "reach.created";
    pub const RESOLUTION: &str = "reach.resolution";
    pub const SCREENS: &str = "reach.screens";
    pub const PROFILE: &str = "reach.profile";
    pub const PROFILE_HOST: &str = "reach.profile_host";
    pub const WORKSPACE: &str = "reach.workspace";

    pub fn for_sandbox(config: &SandboxConfig) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert(Self::MANAGED.into(), "true".into());
        labels.insert(Self::NAME.into(), config.name.clone());
        labels.insert(Self::CREATED.into(), chrono::Utc::now().to_rfc3339());
        labels.insert(Self::RESOLUTION.into(), config.resolution.to_string());
        labels.insert(Self::SCREENS.into(), config.screens.to_string());
        if let Some(profile) = &config.profile {
            labels.insert(Self::PROFILE.into(), profile.name.clone());
            labels.insert(
                Self::PROFILE_HOST.into(),
                profile.host_path.to_string_lossy().into_owned(),
            );
        }
        if let Some(ws) = &config.workspace {
            labels.insert(Self::WORKSPACE.into(), ws.to_string_lossy().into_owned());
        }
        labels
    }

    pub fn filter() -> HashMap<String, Vec<String>> {
        let mut filters = HashMap::new();
        filters.insert("label".into(), vec![format!("{}=true", Self::MANAGED)]);
        filters
    }
}

fn bind(source: &std::path::Path, target: &str) -> Mount {
    Mount {
        target: Some(target.to_string()),
        source: Some(source.to_string_lossy().into_owned()),
        typ: Some(MountTypeEnum::BIND),
        read_only: Some(false),
        ..Default::default()
    }
}

/// Bind mounts for a sandbox: persistent Chrome profile and `/workspace`.
pub fn build_mounts(config: &SandboxConfig) -> Vec<Mount> {
    let mut v = Vec::new();
    if let Some(p) = &config.profile {
        v.push(bind(&p.host_path, &p.container_path));
    }
    if let Some(ws) = &config.workspace {
        v.push(bind(ws, WORKSPACE_CONTAINER_PATH));
    }
    v
}

fn is_known_port(port: u16, screens: u32) -> bool {
    port == 8400
        || (5900..5900 + screens as u16).contains(&port)
        || (6080..6080 + screens as u16).contains(&port)
}

/// Rebuild a [`SandboxConfig`] from a live container's inspect output
/// (labels + `HostConfig`), so `recreate` can tear a container down and
/// recreate it with the same settings and volumes.
///
/// `fallback_profile_dir` is used only when the container predates the
/// `reach.profile_host` label (i.e. it has `reach.profile` but not the
/// host path) — the profile host path is then re-derived from the
/// standard `<fallback_profile_dir>/<name>` layout.
pub fn config_from_inspect(
    resp: &ContainerInspectResponse,
    fallback_profile_dir: &std::path::Path,
) -> Result<SandboxConfig> {
    let cfg = resp
        .config
        .as_ref()
        .context("inspect response missing Config")?;
    let host_config = resp
        .host_config
        .as_ref()
        .context("inspect response missing HostConfig")?;
    let labels = cfg.labels.clone().unwrap_or_default();

    let name = labels
        .get(Labels::NAME)
        .cloned()
        .context("container missing reach.name label")?;
    let image = cfg.image.clone().context("container missing image")?;
    let resolution = labels
        .get(Labels::RESOLUTION)
        .map(|s| Resolution::parse(s))
        .transpose()?
        .context("container missing reach.resolution label")?;
    let screens: u32 = labels
        .get(Labels::SCREENS)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let port_bindings = host_config.port_bindings.clone().unwrap_or_default();
    let host_port = |container_port: &str| -> Option<u16> {
        port_bindings
            .get(container_port)
            .and_then(|v| v.as_ref())
            .and_then(|v| v.first())
            .and_then(|pb| pb.host_port.as_ref())
            .and_then(|p| p.parse().ok())
    };

    let ports = SandboxPorts {
        vnc: host_port("5900/tcp").unwrap_or(5900),
        novnc: host_port("6080/tcp").unwrap_or(6080),
        health: host_port("8400/tcp").unwrap_or(8400),
        extra: port_bindings
            .iter()
            .filter_map(|(container_port, bindings)| {
                let cp: u16 = container_port.trim_end_matches("/tcp").parse().ok()?;
                if is_known_port(cp, screens) {
                    return None;
                }
                let host_port: u16 = bindings
                    .as_ref()?
                    .first()?
                    .host_port
                    .as_ref()?
                    .parse()
                    .ok()?;
                Some((host_port, cp))
            })
            .collect(),
    };

    let memory = host_config.memory.filter(|m| *m > 0).map(|m| m as u64);
    let shm_size = host_config
        .shm_size
        .filter(|s| *s > 0)
        .map(|s| s as u64)
        .unwrap_or(SandboxConfig::default().shm_size);
    let restart_unless_stopped = host_config
        .restart_policy
        .as_ref()
        .and_then(|rp| rp.name)
        .map(|n| n == RestartPolicyNameEnum::UNLESS_STOPPED)
        .unwrap_or(false);

    let workspace = labels.get(Labels::WORKSPACE).map(PathBuf::from);

    let vnc_password = cfg
        .env
        .as_ref()
        .into_iter()
        .flatten()
        .find_map(|e| e.strip_prefix("VNC_PASSWORD=").map(String::from))
        .filter(|s| !s.is_empty());

    let profile = labels.get(Labels::PROFILE).map(|name| {
        let host_path = labels
            .get(Labels::PROFILE_HOST)
            .map(PathBuf::from)
            .unwrap_or_else(|| ProfileMount::host_path_for(fallback_profile_dir, name));
        ProfileMount {
            name: name.clone(),
            host_path,
            container_path: ProfileMount::container_path_for(name),
        }
    });

    Ok(SandboxConfig {
        name,
        image,
        resolution,
        shm_size,
        ports,
        screens,
        profile,
        workspace,
        memory,
        restart_unless_stopped,
        vnc_password,
    })
}

// ═══════════════════════════════════════════════════════════
// Docker client
// ═══════════════════════════════════════════════════════════

pub struct DockerClient {
    client: bollard::Docker,
}

impl DockerClient {
    pub fn new() -> Result<Self> {
        let client = bollard::Docker::connect_with_local_defaults()?;
        Ok(Self { client })
    }

    pub fn inner(&self) -> &bollard::Docker {
        &self.client
    }

    pub async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        let labels = Labels::for_sandbox(&config);

        let port_bindings = {
            let mut map: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
            for i in 0..config.screens {
                let vnc_container = 5900 + i;
                let novnc_container = 6080 + i;
                let vnc_host = config.ports.vnc + i as u16;
                let novnc_host = config.ports.novnc + i as u16;

                map.insert(
                    format!("{vnc_container}/tcp"),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".into()),
                        host_port: Some(vnc_host.to_string()),
                    }]),
                );
                map.insert(
                    format!("{novnc_container}/tcp"),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".into()),
                        host_port: Some(novnc_host.to_string()),
                    }]),
                );
            }
            map.insert(
                "8400/tcp".into(),
                Some(vec![PortBinding {
                    host_ip: Some("0.0.0.0".into()),
                    host_port: Some(config.ports.health.to_string()),
                }]),
            );
            for (host_port, container_port) in &config.ports.extra {
                map.insert(
                    format!("{}/tcp", container_port),
                    Some(vec![PortBinding {
                        host_ip: Some("0.0.0.0".into()),
                        host_port: Some(host_port.to_string()),
                    }]),
                );
            }
            map
        };

        for dir in config
            .profile
            .iter()
            .map(|p| &p.host_path)
            .chain(config.workspace.iter())
        {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create host dir {}", dir.display()))?;
        }
        let mounts = build_mounts(&config);

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            shm_size: Some(config.shm_size as i64),
            memory: config.memory.map(|m| m as i64),
            restart_policy: config.restart_unless_stopped.then_some(
                bollard::models::RestartPolicy {
                    name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                    maximum_retry_count: None,
                },
            ),
            mounts: if mounts.is_empty() {
                None
            } else {
                Some(mounts)
            },
            ..Default::default()
        };

        let mut env = vec![
            format!("WIDTH={}", config.resolution.width),
            format!("HEIGHT={}", config.resolution.height),
            format!("REACH_SCREENS={}", config.screens),
        ];
        if config.workspace.is_some() {
            env.push(format!("REACH_WORKSPACE={WORKSPACE_CONTAINER_PATH}"));
        }
        if let Some(pw) = config.vnc_password.as_deref().filter(|s| !s.is_empty()) {
            env.push(format!("VNC_PASSWORD={pw}"));
        }

        let container_config = Config {
            image: Some(config.image.clone()),
            labels: Some(labels),
            host_config: Some(host_config),
            env: Some(env),
            exposed_ports: Some({
                let mut m = HashMap::new();
                for i in 0..config.screens {
                    m.insert(format!("{}/tcp", 5900 + i), HashMap::new());
                    m.insert(format!("{}/tcp", 6080 + i), HashMap::new());
                }
                m.insert("8400/tcp".into(), HashMap::new());
                for (_, container_port) in &config.ports.extra {
                    m.insert(format!("{}/tcp", container_port), HashMap::new());
                }
                m
            }),
            ..Default::default()
        };

        let opts = CreateContainerOptions {
            name: &config.name,
            platform: None,
        };

        let resp = self
            .client
            .create_container(Some(opts), container_config)
            .await
            .context("failed to create container")?;

        self.client
            .start_container::<String>(&resp.id, None)
            .await
            .context("failed to start container")?;

        tracing::info!(name = config.name, id = &resp.id[..12], "sandbox created");

        Ok(Sandbox {
            name: config.name,
            container_id: resp.id,
            status: SandboxStatus::Starting,
            image: config.image,
            ports: SandboxPortMapping {
                vnc: Some(config.ports.vnc),
                novnc: Some(config.ports.novnc),
                health: Some(config.ports.health),
                screens: config.screens,
                extra: config.ports.extra.clone(),
            },
            created_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub async fn destroy(&self, target: &str) -> Result<()> {
        let sandbox = self.find(target).await?;

        self.client
            .stop_container(&sandbox.container_id, Some(StopContainerOptions { t: 10 }))
            .await
            .context("failed to stop container")?;

        self.client
            .remove_container(
                &sandbox.container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .context("failed to remove container")?;

        tracing::info!(name = sandbox.name, "sandbox destroyed");
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Sandbox>> {
        let opts = ListContainersOptions {
            all: true,
            filters: Labels::filter(),
            ..Default::default()
        };

        let containers = self.client.list_containers(Some(opts)).await?;

        let sandboxes = containers
            .into_iter()
            .map(|c| {
                let labels = c.labels.unwrap_or_default();
                let name = labels
                    .get(Labels::NAME)
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                let status = c
                    .state
                    .as_deref()
                    .map(SandboxStatus::from)
                    .unwrap_or(SandboxStatus::Unknown);

                let screens: u32 = labels
                    .get(Labels::SCREENS)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                let mut ports = extract_ports(&c.ports.unwrap_or_default());
                ports.screens = screens;

                Sandbox {
                    name,
                    container_id: c.id.unwrap_or_default(),
                    status,
                    image: c.image.unwrap_or_default(),
                    ports,
                    created_at: labels.get(Labels::CREATED).cloned().unwrap_or_default(),
                }
            })
            .collect();

        Ok(sandboxes)
    }

    pub async fn find(&self, target: &str) -> Result<Sandbox> {
        let sandboxes = self.list().await?;
        sandboxes
            .into_iter()
            .find(|s| s.name == target || s.container_id.starts_with(target))
            .ok_or_else(|| anyhow::anyhow!("sandbox '{}' not found", target))
    }

    pub async fn exec(&self, target: &str, command: &[String]) -> Result<ExecOutput> {
        let sandbox = self.find(target).await?;
        let cmd: Vec<&str> = command.iter().map(|s| s.as_str()).collect();

        let exec = self
            .client
            .create_exec(
                &sandbox.container_id,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    env: Some(vec!["DISPLAY=:99"]),
                    ..Default::default()
                },
            )
            .await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        if let StartExecResults::Attached { mut output, .. } =
            self.client.start_exec(&exec.id, None).await?
        {
            while let Some(Ok(msg)) = output.next().await {
                match msg {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        let inspect = self.client.inspect_exec(&exec.id).await?;
        let exit_code = inspect.exit_code.unwrap_or(-1);

        Ok(ExecOutput {
            exit_code,
            stdout,
            stderr,
        })
    }

    pub async fn screenshot(&self, target: &str, display: &str) -> Result<Vec<u8>> {
        let out = self
            .exec(
                target,
                &[
                    "bash".into(),
                    "-c".into(),
                    format!(
                        "DISPLAY={display} scrot -z /tmp/_reach_shot.png && base64 -w 0 /tmp/_reach_shot.png && rm /tmp/_reach_shot.png"
                    ),
                ],
            )
            .await?;

        if out.exit_code != 0 {
            bail!("screenshot failed: {}", out.stderr);
        }

        use base64::Engine;
        // Defensive: strip any whitespace in case `base64 -w 0` is unavailable
        // and the CLI falls back to line-wrapped output.
        let clean: String = out.stdout.chars().filter(|c| !c.is_whitespace()).collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&clean)
            .context("failed to decode screenshot base64")?;

        Ok(bytes)
    }

    /// Run a Playwright-driven "navigate and extract text" script in the
    /// sandbox.
    ///
    /// The Python helper launches headed Chromium on Xvfb (so the page is
    /// visible through noVNC) and prints a single JSON object on stdout.
    pub async fn page_text(&self, target: &str, opts: &PageTextOptions) -> Result<PageTextOutput> {
        let payload = serde_json::json!({
            "url": opts.url,
            "wait_for": opts.wait_for,
            "selector": opts.selector,
            "timeout_ms": opts.timeout_ms,
            "user_data_dir": opts.user_data_dir,
        });

        let payload_str =
            serde_json::to_string(&payload).context("failed to serialize page_text payload")?;

        // Pass the JSON via stdin-style env var to dodge shell quoting hell.
        let display_prefix = opts
            .display
            .as_deref()
            .map(|d| format!("DISPLAY={d} "))
            .unwrap_or_default();
        let cmd = format!(
            "{display_prefix}REACH_PAGE_TEXT_PAYLOAD={} python3 -c {}",
            shell_single_quote(&payload_str),
            shell_single_quote(PAGE_TEXT_SCRIPT),
        );

        let out = self
            .exec(target, &["bash".into(), "-c".into(), cmd])
            .await?;

        if out.exit_code != 0 {
            // Even with a non-zero exit, the script may have produced JSON.
            if let Some(parsed) = parse_page_text_json(&out.stdout) {
                return Ok(parsed);
            }
            bail!(
                "page_text exec failed (exit {}): {}",
                out.exit_code,
                out.stderr
            );
        }

        parse_page_text_json(&out.stdout).ok_or_else(|| {
            anyhow::anyhow!("page_text returned malformed output: {}", out.stdout.trim())
        })
    }

    /// Open a URL in the sandbox Chrome and (optionally) poll for a
    /// post-auth signal.
    ///
    /// Returns immediately with `status = "auth_required"` and the noVNC
    /// URL if no `wait_for_*` condition is set; otherwise it polls inside
    /// the container until the condition is met or `timeout_seconds`
    /// elapses.
    pub async fn auth_handoff(
        &self,
        target: &str,
        opts: &AuthHandoffOptions,
    ) -> Result<AuthHandoffOutput> {
        let payload = serde_json::json!({
            "url": opts.url,
            "wait_for_selector": opts.wait_for_selector,
            "wait_for_url_contains": opts.wait_for_url_contains,
            "timeout_seconds": opts.timeout_seconds,
            "user_data_dir": opts.user_data_dir,
            "headless": false,
        });

        let payload_str =
            serde_json::to_string(&payload).context("failed to serialize auth_handoff payload")?;

        let display_prefix = opts
            .display
            .as_deref()
            .map(|d| format!("DISPLAY={d} "))
            .unwrap_or_default();
        let cmd = format!(
            "{display_prefix}REACH_AUTH_HANDOFF_PAYLOAD={} python3 -c {}",
            shell_single_quote(&payload_str),
            shell_single_quote(AUTH_HANDOFF_SCRIPT),
        );

        let out = self
            .exec(target, &["bash".into(), "-c".into(), cmd])
            .await?;

        if let Some(parsed) = parse_auth_handoff_json(&out.stdout) {
            return Ok(parsed);
        }

        if out.exit_code != 0 {
            bail!(
                "auth_handoff exec failed (exit {}): {}",
                out.exit_code,
                out.stderr
            );
        }

        Err(anyhow::anyhow!(
            "auth_handoff returned malformed output: {}",
            out.stdout.trim()
        ))
    }

    /// Rebuild the [`SandboxConfig`] that produced `target`, reading it back
    /// from Docker's inspect output (labels + `HostConfig`).
    pub async fn inspect_config(&self, target: &str) -> Result<SandboxConfig> {
        let sandbox = self.find(target).await?;
        let resp = self
            .client
            .inspect_container(&sandbox.container_id, None)
            .await
            .context("failed to inspect container")?;
        let fallback_profile_dir = crate::config::ReachConfig::load()
            .sandbox
            .resolved_profile_dir();
        config_from_inspect(&resp, &fallback_profile_dir)
    }

    /// Recreate `target`: read back its config and volumes, destroy the
    /// container, and create a fresh one with the same settings (host
    /// bind mounts for `/workspace` and any persisted profile are
    /// untouched, since `destroy` only removes the container).
    pub async fn recreate(&self, target: &str, image: Option<String>) -> Result<Sandbox> {
        let mut config = self.inspect_config(target).await?;
        if let Some(image) = image {
            config.image = image;
        }
        let name = config.name.clone();
        self.destroy(target).await?;
        self.create(config).await.with_context(|| {
            format!(
                "destroyed sandbox '{name}' but failed to recreate it; its workspace and \
                 profile directories are intact — rerun `reach recreate` with a valid --image, \
                 or `reach create --name {name} ...`"
            )
        })
    }

    pub async fn wait_healthy(&self, target: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() > deadline {
                bail!("timeout waiting for sandbox '{}' to become healthy", target);
            }

            let out = self
                .exec(
                    target,
                    &[
                        "curl".into(),
                        "-sf".into(),
                        "http://localhost:8400/health".into(),
                    ],
                )
                .await;

            if let Ok(result) = out
                && result.exit_code == 0
            {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn extract_ports(ports: &[bollard::models::Port]) -> SandboxPortMapping {
    let mut mapping = SandboxPortMapping {
        vnc: None,
        novnc: None,
        health: None,
        screens: 1,
        extra: Vec::new(),
    };

    for p in ports {
        match p.private_port {
            5900 => mapping.vnc = p.public_port,
            6080 => mapping.novnc = p.public_port,
            8400 => mapping.health = p.public_port,
            other => {
                if let Some(host_port) = p.public_port {
                    mapping.extra.push((host_port, other));
                }
            }
        }
    }

    mapping
}

// ═══════════════════════════════════════════════════════════
// page_text + auth_handoff: types, helpers, embedded Python
// ═══════════════════════════════════════════════════════════

/// Inputs to [`DockerClient::page_text`].
#[derive(Debug, Clone, Default)]
pub struct PageTextOptions {
    pub url: String,
    pub wait_for: Option<String>,
    pub selector: Option<String>,
    pub timeout_ms: u64,
    /// Persistent Chrome user data dir inside the container.
    pub user_data_dir: Option<String>,
    pub display: Option<String>,
}

/// Parsed output from the embedded `page_text` Python helper.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PageTextOutput {
    pub status: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Inputs to [`DockerClient::auth_handoff`].
#[derive(Debug, Clone, Default)]
pub struct AuthHandoffOptions {
    pub url: String,
    pub wait_for_selector: Option<String>,
    pub wait_for_url_contains: Option<String>,
    pub timeout_seconds: u64,
    pub user_data_dir: Option<String>,
    pub display: Option<String>,
}

/// Parsed output from the embedded `auth_handoff` Python helper.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AuthHandoffOutput {
    pub status: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Quote a string so it survives a single-quoted bash word.
///
/// Replaces every `'` with `'\''` and wraps the result in single quotes.
pub(crate) fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build a noVNC URL for a sandbox given its mapped port.
pub fn novnc_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}/vnc.html?autoconnect=1&resize=remote")
}

fn parse_page_text_json(stdout: &str) -> Option<PageTextOutput> {
    last_json_line(stdout).and_then(|l| serde_json::from_str(&l).ok())
}

fn parse_auth_handoff_json(stdout: &str) -> Option<AuthHandoffOutput> {
    last_json_line(stdout).and_then(|l| serde_json::from_str(&l).ok())
}

/// Find the last non-empty line in `stdout` that looks like a JSON object.
///
/// The Python helpers may print warnings on stdout (Playwright, etc.)
/// before the result line, so we scan from the bottom.
fn last_json_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('{') && l.ends_with('}'))
        .map(|l| l.to_string())
}

/// Embedded Playwright "navigate and extract text" helper.
///
/// Reads its JSON payload from `REACH_PAGE_TEXT_PAYLOAD` and prints a
/// single-line JSON object on stdout. Always exits 0 so the caller can
/// distinguish "Python ran but the page failed to load" from "exec
/// failed entirely".
pub const PAGE_TEXT_SCRIPT: &str = r#"
import json
import os
import sys

payload = json.loads(os.environ.get("REACH_PAGE_TEXT_PAYLOAD", "{}"))
url = payload.get("url")
wait_for = payload.get("wait_for")
selector = payload.get("selector")
timeout_ms = int(payload.get("timeout_ms") or 30000)
user_data_dir = payload.get("user_data_dir")

if not url:
    print(json.dumps({"status": "error", "message": "missing url"}))
    sys.exit(0)

try:
    from playwright.sync_api import sync_playwright
except Exception as exc:  # pragma: no cover
    print(json.dumps({"status": "error", "message": f"playwright import failed: {exc}"}))
    sys.exit(0)

os.environ.setdefault("DISPLAY", ":99")

try:
    with sync_playwright() as p:
        if user_data_dir:
            os.makedirs(user_data_dir, exist_ok=True)
            ctx = p.chromium.launch_persistent_context(
                user_data_dir=user_data_dir,
                headless=False,
                args=["--no-sandbox", "--disable-gpu", "--no-first-run"],
            )
            has_cookies = any(os.path.exists(os.path.join(user_data_dir, sub)) for sub in ["Default/Network/Cookies", "Default/Cookies", "Cookies"])
            state_file = "/workspace/.reach/state.json"
            if not has_cookies and os.path.exists(state_file):
                try:
                    with open(state_file) as f:
                        state = json.load(f)
                    cookies = state.get("cookies", [])
                    if cookies:
                        import time
                        now = time.time()
                        for c in cookies:
                            if c.get("expires", -1) <= 0:
                                c["expires"] = int(now + 86400 * 30)
                        ctx.add_cookies(cookies)
                except Exception:
                    pass
            page = ctx.new_page() if not ctx.pages else ctx.pages[0]
            owner = ctx
        else:
            browser = p.chromium.launch(
                headless=False,
                args=["--no-sandbox", "--disable-gpu", "--no-first-run"],
            )
            state_file = "/workspace/.reach/state.json"
            if os.path.exists(state_file):
                try:
                    ctx = browser.new_context(storage_state=state_file)
                except Exception:
                    ctx = browser.new_context()
            else:
                ctx = browser.new_context()
            page = ctx.new_page()
            owner = browser

        try:
            page.goto(url, timeout=timeout_ms, wait_until="domcontentloaded")
            if wait_for:
                page.wait_for_selector(wait_for, timeout=timeout_ms)
            else:
                try:
                    page.wait_for_load_state("networkidle", timeout=timeout_ms)
                except Exception:
                    pass

            if selector:
                el = page.query_selector(selector)
                text = el.inner_text() if el else ""
            else:
                text = page.locator("body").inner_text()

            result = {
                "status": "ok",
                "url": page.url,
                "title": page.title(),
                "text": text,
            }
        finally:
            try:
                owner.close()
            except Exception:
                pass
except Exception as exc:
    result = {"status": "error", "message": str(exc)}

print(json.dumps(result))
"#;

/// Embedded Playwright auth-handoff helper.
///
/// Launches a persistent Chromium context (so the user can log in via
/// noVNC), then either returns immediately or polls for a selector / URL
/// substring before returning.
pub const AUTH_HANDOFF_SCRIPT: &str = r#"
import json
import os
import sys
import time

payload = json.loads(os.environ.get("REACH_AUTH_HANDOFF_PAYLOAD", "{}"))
url = payload.get("url")
wait_for_selector = payload.get("wait_for_selector")
wait_for_url_contains = payload.get("wait_for_url_contains")
timeout_seconds = int(payload.get("timeout_seconds") or 300)
user_data_dir = payload.get("user_data_dir") or "/home/sandbox/.config/google-chrome-profiles/default"

if not url:
    print(json.dumps({"status": "error", "message": "missing url"}))
    sys.exit(0)

try:
    from playwright.sync_api import sync_playwright
except Exception as exc:  # pragma: no cover
    print(json.dumps({"status": "error", "message": f"playwright import failed: {exc}"}))
    sys.exit(0)

os.environ.setdefault("DISPLAY", ":99")
os.makedirs(user_data_dir, exist_ok=True)

needs_wait = bool(wait_for_selector or wait_for_url_contains)

try:
    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            user_data_dir=user_data_dir,
            headless=False,
            args=["--no-sandbox", "--disable-gpu", "--no-first-run"],
        )
        has_cookies = any(os.path.exists(os.path.join(user_data_dir, sub)) for sub in ["Default/Network/Cookies", "Default/Cookies", "Cookies"])
        state_file = "/workspace/.reach/state.json"
        if not has_cookies and os.path.exists(state_file):
            try:
                with open(state_file) as f:
                    state = json.load(f)
                cookies = state.get("cookies", [])
                if cookies:
                    now = time.time()
                    for c in cookies:
                        if c.get("expires", -1) <= 0:
                            c["expires"] = int(now + 86400 * 30)
                    ctx.add_cookies(cookies)
            except Exception:
                pass

        page = ctx.new_page() if not ctx.pages else ctx.pages[0]

        try:
            page.goto(url, timeout=30000, wait_until="domcontentloaded")
        except Exception as exc:
            result = {
                "status": "error",
                "message": f"navigation failed: {exc}",
                "url": url,
            }
            ctx.close()
            print(json.dumps(result))
            sys.exit(0)

        if not needs_wait:
            # Detach so the browser keeps running for the human.
            print(json.dumps({
                "status": "auth_required",
                "url": page.url,
                "message": "Open the noVNC URL to log in. Re-call once done.",
            }))
            sys.exit(0)

        deadline = time.time() + timeout_seconds
        matched = False
        while time.time() < deadline:
            try:
                if wait_for_url_contains and wait_for_url_contains in page.url:
                    matched = True
                    break
                if wait_for_selector and page.query_selector(wait_for_selector):
                    matched = True
                    break
            except Exception:
                pass
            time.sleep(1)

        if matched:
            try:
                os.makedirs("/workspace/.reach", exist_ok=True)
                ctx.storage_state(path="/workspace/.reach/state.json")
            except Exception:
                pass

        result = {
            "status": "authenticated" if matched else "timeout",
            "url": page.url,
            "message": None if matched else "auth signal not seen before timeout",
        }
        ctx.close()
except Exception as exc:
    result = {"status": "error", "message": str(exc)}

print(json.dumps(result))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_handles_quotes() {
        assert_eq!(shell_single_quote("hello"), "'hello'");
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_single_quote("a 'b' c"), "'a '\\''b'\\'' c'");
    }

    #[test]
    fn last_json_line_picks_trailing_object() {
        let stdout = "warning: foo\nINFO: bar\n{\"status\":\"ok\",\"text\":\"hi\"}\n";
        assert_eq!(
            last_json_line(stdout).as_deref(),
            Some("{\"status\":\"ok\",\"text\":\"hi\"}")
        );
    }

    #[test]
    fn last_json_line_returns_none_when_absent() {
        assert!(last_json_line("no json here\nstill nothing").is_none());
    }

    #[test]
    fn parse_page_text_json_round_trip() {
        let stdout = "noise\n{\"status\":\"ok\",\"text\":\"hello\",\"url\":\"https://x\"}\n";
        let parsed = parse_page_text_json(stdout).unwrap();
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.text.as_deref(), Some("hello"));
        assert_eq!(parsed.url.as_deref(), Some("https://x"));
    }

    #[test]
    fn parse_auth_handoff_json_round_trip() {
        let stdout = "{\"status\":\"auth_required\",\"url\":\"https://x\"}";
        let parsed = parse_auth_handoff_json(stdout).unwrap();
        assert_eq!(parsed.status, "auth_required");
        assert_eq!(parsed.url.as_deref(), Some("https://x"));
    }

    #[test]
    fn novnc_url_format() {
        assert_eq!(
            novnc_url("localhost", 6080),
            "http://localhost:6080/vnc.html?autoconnect=1&resize=remote"
        );
    }

    #[test]
    fn profile_mount_paths() {
        assert_eq!(
            ProfileMount::container_path_for("personal"),
            "/home/sandbox/.config/google-chrome-profiles/personal"
        );
        let base = std::path::Path::new("/tmp/reach/profiles");
        let host = ProfileMount::host_path_for(base, "personal");
        assert_eq!(
            host,
            std::path::PathBuf::from("/tmp/reach/profiles/personal")
        );
    }
}
