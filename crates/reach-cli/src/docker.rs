use anyhow::{Context, Result, bail};
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, NetworkingConfig,
    RemoveContainerOptions, StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{
    ContainerInspectResponse, EndpointSettings, HostConfig, Mount, MountTypeEnum, PortBinding,
    RestartPolicyNameEnum,
};
use bollard::network::CreateNetworkOptions;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

// ═══════════════════════════════════════════════════════════
// Sandbox configuration
// ═══════════════════════════════════════════════════════════

/// Path inside the container where the durable workspace mount lands.
pub const WORKSPACE_CONTAINER_PATH: &str = "/workspace";

#[derive(Clone)]
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
    ///
    /// Note: because the password is passed via the container's `VNC_PASSWORD`
    /// environment variable, it is visible to any process or user with access
    /// to `docker inspect`.
    pub vnc_password: Option<String>,
    /// Whether the sandbox container allows arbitrary shell command execution via the `exec` tool.
    pub allow_exec: bool,
    /// Whether `/workspace` is mounted read-write. When false, `/workspace` is mounted read-only.
    pub writable_workspace: bool,
}

/// Validate security-sensitive combinations before any Docker resources are
/// allocated.
pub fn validate_sandbox_config(config: &SandboxConfig) -> Result<()> {
    if config.allow_exec && config.profile.is_some() {
        bail!(
            "code-capable sandboxes cannot mount personal browser profiles; \
             use a clean code workspace or disable --allow-exec"
        );
    }
    Ok(())
}

/// Lifecycle state recorded for reset/recovery decisions.
///
/// `Hydrated` describes a one-shot runtime cookie/session hydration. It is
/// deliberately not persisted in a reset manifest; `Persistent` only records
/// host-backed workspace/profile mounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleMode {
    Clean,
    Hydrated,
    Persistent,
}

/// Metadata safe to persist for a reset/recovery operation.
///
/// This intentionally excludes VNC passwords, hydrated cookies, capabilities,
/// element refs, and other runtime credentials. A clean clone receives no
/// capability/ref state from the source.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResetManifest {
    pub schema_version: u32,
    pub name: String,
    pub image: String,
    pub mode: LifecycleMode,
    pub workspace: Option<PathBuf>,
    pub profile_name: Option<String>,
    pub writable_workspace: bool,
    pub restart_unless_stopped: bool,
}

pub const RESET_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Resolve the lifecycle mode without inspecting runtime state.
pub fn lifecycle_mode(
    profile: Option<&ProfileMount>,
    hydrated: bool,
    persistent_workspace: bool,
) -> LifecycleMode {
    if profile.is_some() || persistent_workspace {
        LifecycleMode::Persistent
    } else if hydrated {
        LifecycleMode::Hydrated
    } else {
        LifecycleMode::Clean
    }
}

/// Build a reset manifest from the existing create/recreate configuration.
/// Runtime credentials and old capability/ref state are intentionally omitted.
pub fn reset_manifest_for(config: &SandboxConfig) -> ResetManifest {
    ResetManifest {
        schema_version: RESET_MANIFEST_SCHEMA_VERSION,
        name: config.name.clone(),
        image: config.image.clone(),
        mode: lifecycle_mode(config.profile.as_ref(), false, config.workspace.is_some()),
        workspace: config.workspace.clone(),
        profile_name: config.profile.as_ref().map(|profile| profile.name.clone()),
        writable_workspace: config.writable_workspace,
        restart_unless_stopped: config.restart_unless_stopped,
    }
}

/// Place a manifest beside a workspace or under a caller-provided state root.
pub fn reset_manifest_path(root: &std::path::Path, name: &str) -> PathBuf {
    root.join(".reach").join(format!("{name}.reset.json"))
}

/// Persist a reset manifest with private permissions and an atomic rename.
pub fn write_reset_manifest(path: &std::path::Path, manifest: &ResetManifest) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path
        .parent()
        .context("reset manifest has no parent directory")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create reset manifest directory {}",
            parent.display()
        )
    })?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
        || {
            format!(
                "failed to protect reset manifest directory {}",
                parent.display()
            )
        },
    )?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("reset"),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));

    let bytes =
        serde_json::to_vec_pretty(manifest).context("failed to serialize reset manifest")?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("failed to create private reset manifest {}", tmp.display()))?;
    file.write_all(&bytes)
        .context("failed to write reset manifest")?;
    file.sync_all().context("failed to sync reset manifest")?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to atomically install reset manifest {}",
            path.display()
        )
    })?;
    Ok(())
}
/// Read a previously written recovery manifest without hydrating credentials.
pub fn read_reset_manifest(path: &std::path::Path) -> Result<ResetManifest> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read reset manifest {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse reset manifest {}", path.display()))
}

impl std::fmt::Debug for SandboxConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxConfig")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("resolution", &self.resolution)
            .field("shm_size", &self.shm_size)
            .field("ports", &self.ports)
            .field("screens", &self.screens)
            .field("profile", &self.profile)
            .field("workspace", &self.workspace)
            .field("memory", &self.memory)
            .field("restart_unless_stopped", &self.restart_unless_stopped)
            .field(
                "vnc_password",
                &self.vnc_password.as_ref().map(|_| "<redacted>"),
            )
            .field("allow_exec", &self.allow_exec)
            .field("writable_workspace", &self.writable_workspace)
            .finish()
    }
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
            allow_exec: false,
            writable_workspace: false,
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
    #[serde(default)]
    pub allow_exec: bool,
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
    pub const ALLOW_EXEC: &str = "reach.allow_exec";
    pub const WRITABLE_WORKSPACE: &str = "reach.writable_workspace";
    pub const NETWORK: &str = "reach.network";
    pub const NETWORK_MANAGED: &str = "reach.network.managed";

    pub fn for_sandbox(config: &SandboxConfig) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        labels.insert(Self::MANAGED.into(), "true".into());
        labels.insert(Self::NAME.into(), config.name.clone());
        labels.insert(Self::CREATED.into(), chrono::Utc::now().to_rfc3339());
        labels.insert(Self::RESOLUTION.into(), config.resolution.to_string());
        labels.insert(Self::SCREENS.into(), config.screens.to_string());
        labels.insert(Self::ALLOW_EXEC.into(), config.allow_exec.to_string());
        labels.insert(
            Self::WRITABLE_WORKSPACE.into(),
            config.writable_workspace.to_string(),
        );
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

fn network_name_for(container_name: &str) -> String {
    let safe: String = container_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    format!("reach-{safe}-{}", uuid::Uuid::new_v4().simple())
}

pub fn screen_display(screen: u32) -> String {
    format!(":{}", 99u32.saturating_add(screen))
}

pub fn screen_cdp_port(screen: u32) -> Result<u16> {
    9222u32
        .checked_add(screen)
        .and_then(|port| u16::try_from(port).ok())
        .context("screen CDP port exceeds u16")
}

/// Fixed in-container cleanup helper. It only considers a browser that is a
/// session leader, has the exact screen environment and exact CDP argv token.
/// The process group is killed only after every member is proven to belong to
/// that session; any uncertainty fails closed.
pub const RESET_SCREEN_SCRIPT: &str = r#"
import os, signal, subprocess, sys, time

screen = int(sys.argv[1])
display = ':{}'.format(99 + screen)
port = str(9222 + screen)

def proc(pid):
    try:
        with open('/proc/{}/cmdline'.format(pid), 'rb') as f:
            argv = [x.decode(errors='replace') for x in f.read().split(b'\0') if x]
        with open('/proc/{}/environ'.format(pid), 'rb') as f:
            env = dict(x.split(b'=', 1) for x in f.read().split(b'\0') if b'=' in x)
        with open('/proc/{}/stat'.format(pid)) as f:
            stat = f.read().split()
        return argv, env, int(stat[3]), int(stat[4]), int(stat[5])
    except (FileNotFoundError, PermissionError, ProcessLookupError, ValueError):
        return None
rows = {}
for entry in os.listdir('/proc'):
    if entry.isdigit():
        row = proc(int(entry))
        if row is not None:
            rows[int(entry)] = row

roots = []
for pid, (argv, env, ppid, pgid, sid) in rows.items():
    if not argv:
        continue
    if sid != pid or pgid != pid:
        continue
    if env.get(b'DISPLAY', b'').decode(errors='replace') != display:
        continue
    if os.path.basename(argv[0]).lower() not in ('chrome', 'google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser'):
        continue
    if '--remote-debugging-port=' + port not in argv:
        continue
    roots.append(pid)

if len(roots) > 1:
    raise SystemExit('multiple exact browser session leaders; refusing cleanup')

if roots:
    root = roots[0]
    descendants = {root}
    changed = True
    while changed:
        changed = False
        for pid, (_, _, ppid, pgid, sid) in rows.items():
            if ppid in descendants and pid not in descendants:
                descendants.add(pid)
                changed = True
    for pid, (_, _, ppid, pgid, sid) in rows.items():
        if pgid == root and pid not in descendants:
            raise SystemExit('browser process group ownership is ambiguous')
    os.killpg(root, signal.SIGTERM)
    deadline = time.time() + 2.0
    while time.time() < deadline:
        if all(proc(pid) is None for pid in descendants):
            break
        time.sleep(0.05)
    if any(proc(pid) is not None for pid in descendants):
        os.killpg(root, signal.SIGKILL)
        time.sleep(0.1)
    if any(proc(pid) is not None for pid in descendants):
        raise SystemExit('browser process tree did not terminate')
legacy_ref = '/workspace/.reach/refs/screen_{}.json'.format(screen)
try:
    os.unlink(legacy_ref)
except FileNotFoundError:
    pass
except OSError:
    raise SystemExit('failed to remove legacy screen refs')
env = os.environ.copy()
env['DISPLAY'] = display
try:
    for selection in ('clipboard', 'primary'):
        # xclip's selection-owner child keeps inherited pipes open after its
        # launcher exits. Do not wait for that child's stderr to reach EOF.
        result = subprocess.run(
            ['xclip', '-selection', selection, '-in'],
            input=b'', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env=env, check=False, timeout=5,
        )
        if result.returncode != 0:
            raise SystemExit('failed to clear screen clipboard')
except subprocess.TimeoutExpired:
    raise SystemExit('screen clipboard cleanup timed out')
"#;

fn bind(source: &std::path::Path, target: &str) -> Mount {
    Mount {
        target: Some(target.to_string()),
        source: Some(source.to_string_lossy().into_owned()),
        typ: Some(MountTypeEnum::BIND),
        read_only: Some(false),
        ..Default::default()
    }
}

fn bind_ro(source: &std::path::Path, target: &str, read_only: bool) -> Mount {
    Mount {
        target: Some(target.to_string()),
        source: Some(source.to_string_lossy().into_owned()),
        typ: Some(MountTypeEnum::BIND),
        read_only: Some(read_only),
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
        v.push(bind_ro(
            ws,
            WORKSPACE_CONTAINER_PATH,
            !config.writable_workspace,
        ));
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

    let allow_exec = labels
        .get(Labels::ALLOW_EXEC)
        .map(|s| s == "true")
        .unwrap_or(false);
    let writable_workspace = labels
        .get(Labels::WRITABLE_WORKSPACE)
        .map(|s| s == "true")
        .unwrap_or(false);

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
        allow_exec,
        writable_workspace,
    })
}

// ═══════════════════════════════════════════════════════════
// Docker client
// ═══════════════════════════════════════════════════════════

/// Resolve the Docker socket/host address by priority:
/// 1. Explicit socket from config or CLI (if Some and non-empty)
/// 2. `DOCKER_HOST` environment variable (if set and non-empty)
/// 3. Default local socket (returns None)
pub fn resolve_docker_socket(explicit: Option<&str>) -> Option<String> {
    resolve_docker_socket_with_env(explicit, std::env::var("DOCKER_HOST").ok().as_deref())
}

pub fn resolve_docker_socket_with_env(
    explicit: Option<&str>,
    env_docker_host: Option<&str>,
) -> Option<String> {
    if let Some(s) = explicit {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(host) = env_docker_host {
        let trimmed = host.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

pub struct DockerClient {
    client: bollard::Docker,
}

impl DockerClient {
    pub fn new(socket: Option<&str>) -> Result<Self> {
        let resolved = resolve_docker_socket(socket);
        let client = match resolved {
            Some(ref addr) if addr.starts_with("tcp://") || addr.starts_with("http://") => {
                bollard::Docker::connect_with_http(addr, 120, bollard::API_DEFAULT_VERSION)?
            }
            Some(ref addr) => {
                bollard::Docker::connect_with_socket(addr, 120, bollard::API_DEFAULT_VERSION)?
            }
            None => bollard::Docker::connect_with_local_defaults()?,
        };
        Ok(Self { client })
    }

    pub fn inner(&self) -> &bollard::Docker {
        &self.client
    }

    pub async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        validate_sandbox_config(&config)?;
        let network_name = network_name_for(&config.name);
        let mut labels = Labels::for_sandbox(&config);
        labels.insert(Labels::NETWORK.into(), network_name.clone());
        let mut network_labels = HashMap::new();
        network_labels.insert(Labels::NETWORK_MANAGED.to_string(), "true".to_string());
        network_labels.insert(Labels::NAME.to_string(), config.name.clone());
        self.client
            .create_network(CreateNetworkOptions {
                name: network_name.clone(),
                check_duplicate: true,
                driver: "bridge".to_string(),
                internal: false,
                attachable: false,
                ingress: false,
                ipam: Default::default(),
                enable_ipv6: false,
                options: HashMap::from([(
                    "com.docker.network.bridge.enable_icc".to_string(),
                    "false".to_string(),
                )]),
                labels: network_labels,
            })
            .await
            .context("failed to create isolated sandbox bridge; backend may not support ICC-disabled bridge networks")?;
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
                        host_ip: Some("127.0.0.1".into()),
                        host_port: Some(vnc_host.to_string()),
                    }]),
                );
                map.insert(
                    format!("{novnc_container}/tcp"),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".into()),
                        host_port: Some(novnc_host.to_string()),
                    }]),
                );
            }
            map.insert(
                "8400/tcp".into(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".into()),
                    host_port: Some(config.ports.health.to_string()),
                }]),
            );
            for (host_port, container_port) in &config.ports.extra {
                map.insert(
                    format!("{}/tcp", container_port),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".into()),
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
            if let Err(error) = std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create host dir {}", dir.display()))
            {
                let _ = self.client.remove_network(&network_name).await;
                return Err(error);
            }
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
            cap_drop: Some(vec!["ALL".to_string()]),
            security_opt: Some(vec!["no-new-privileges:true".to_string()]),
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
            networking_config: Some(NetworkingConfig {
                endpoints_config: HashMap::from([(
                    network_name.clone(),
                    EndpointSettings::default(),
                )]),
            }),
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

        let resp = match self
            .client
            .create_container(Some(opts), container_config)
            .await
            .context("failed to create container")
        {
            Ok(resp) => resp,
            Err(error) => {
                let _ = self.client.remove_network(&network_name).await;
                return Err(error);
            }
        };

        if let Err(error) = self
            .client
            .start_container::<String>(&resp.id, None)
            .await
            .context("failed to start container")
        {
            let _ = self
                .client
                .remove_container(
                    &resp.id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            let _ = self.client.remove_network(&network_name).await;
            return Err(error);
        }
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
            allow_exec: config.allow_exec,
        })
    }

    pub async fn destroy(&self, target: &str) -> Result<()> {
        let sandbox = self.find(target).await?;
        let network_name = self
            .client
            .inspect_container(&sandbox.container_id, None)
            .await
            .ok()
            .and_then(|inspect| {
                inspect
                    .config
                    .and_then(|config| config.labels)
                    .and_then(|labels| labels.get(Labels::NETWORK).cloned())
            });

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

        if let Some(network_name) = network_name
            && let Ok(network) = self
                .client
                .inspect_network::<String>(&network_name, None)
                .await
        {
            let owned = network.name.as_deref() == Some(network_name.as_str())
                && network
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(Labels::NETWORK_MANAGED))
                    .map(String::as_str)
                    == Some("true");
            if owned {
                self.client
                    .remove_network(&network_name)
                    .await
                    .context("failed to remove isolated sandbox bridge")?;
            }
        }

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
                let allow_exec = labels
                    .get(Labels::ALLOW_EXEC)
                    .map(|s| s == "true")
                    .unwrap_or(false);
                let mut ports = extract_ports(&c.ports.unwrap_or_default());
                ports.screens = screens;

                Sandbox {
                    name,
                    container_id: c.id.unwrap_or_default(),
                    status,
                    image: c.image.unwrap_or_default(),
                    ports,
                    created_at: labels.get(Labels::CREATED).cloned().unwrap_or_default(),
                    allow_exec,
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
        let shot_id = uuid::Uuid::new_v4().simple();
        let disp_clean = display.replace(':', "_");
        let shot_file = format!("/tmp/_reach_shot_{disp_clean}_{shot_id}.png");
        let out = self
            .exec(
                target,

                &[
                    "bash".into(),
                    "-c".into(),
                    format!(
                        "DISPLAY={display} scrot -z '{shot_file}' && base64 -w 0 '{shot_file}' && rm -f '{shot_file}'"
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
    /// Return the container's current identity. A restart changes StartedAt,
    /// so this value is not a stable container-name alias.
    pub async fn incarnation(&self, target: &str) -> Result<String> {
        let sandbox = self.find(target).await?;
        let inspect = self
            .client
            .inspect_container(&sandbox.container_id, None)
            .await
            .context("failed to inspect container incarnation")?;
        let id = inspect
            .id
            .context("inspect response missing container ID")?;
        let started_at = inspect
            .state
            .and_then(|state| state.started_at)
            .filter(|value| !value.is_empty())
            .context("inspect response missing container StartedAt")?;
        Ok(format!("{id}:{started_at}"))
    }

    /// Attach sensitive input to a command over stdin, never argv or env.
    pub async fn exec_input(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput> {
        use tokio::io::AsyncWriteExt;
        let sandbox = self.find(target).await?;
        let cmd: Vec<&str> = command.iter().map(String::as_str).collect();
        let exec = self
            .client
            .create_exec(
                &sandbox.container_id,
                CreateExecOptions {
                    cmd: Some(cmd),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .context("failed to create stdin-attached exec")?;
        let started = self
            .client
            .start_exec(&exec.id, None)
            .await
            .context("failed to start stdin-attached exec")?;
        let (write_result, drain_result) = match started {
            StartExecResults::Attached {
                mut output,
                input: mut stdin,
            } => {
                let write_future = async move {
                    stdin.write_all(input).await?;
                    stdin.shutdown().await
                };
                let drain_future = async move {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    while let Some(message) = output.next().await {
                        match message? {
                            bollard::container::LogOutput::StdOut { message } => {
                                stdout.push_str(&String::from_utf8_lossy(&message));
                            }
                            bollard::container::LogOutput::StdErr { message } => {
                                stderr.push_str(&String::from_utf8_lossy(&message));
                            }
                            _ => {}
                        }
                    }
                    Ok::<_, bollard::errors::Error>((stdout, stderr))
                };
                tokio::join!(write_future, drain_future)
            }
            StartExecResults::Detached => {
                bail!("stdin-attached exec unexpectedly detached");
            }
        };
        write_result.context("failed to send stdin-attached exec input")?;
        let (stdout, stderr) =
            drain_result.context("failed to drain stdin-attached exec output")?;
        let inspect = self.client.inspect_exec(&exec.id).await?;
        Ok(ExecOutput {
            exit_code: inspect.exit_code.unwrap_or(-1),
            stdout,
            stderr,
        })
    }

    /// Reset only the browser process session and clipboard for one screen.
    /// Persistent profile files remain untouched.
    pub async fn reset_screen(&self, target: &str, screen: u32) -> Result<()> {
        let _ = screen_cdp_port(screen)?;
        let output = self
            .exec(
                target,
                &[
                    "python3".into(),
                    "-c".into(),
                    RESET_SCREEN_SCRIPT.into(),
                    screen.to_string(),
                ],
            )
            .await?;
        if output.exit_code != 0 {
            bail!(
                "screen reset failed closed for screen {screen}: {}",
                output.stderr.trim()
            );
        }
        Ok(())
    }

    /// Run a Playwright-driven "navigate and extract text" script in the
    /// sandbox.
    ///
    /// The Python helper launches headed Chromium on Xvfb (so the page is
    /// visible through noVNC) and prints a single JSON object on stdout.
    pub async fn page_text(&self, target: &str, opts: &PageTextOptions) -> Result<PageTextOutput> {
        let screen_num: u32 = opts
            .display
            .as_deref()
            .and_then(|d| d.strip_prefix(':'))
            .and_then(|n| n.parse().ok())
            .map(|d: u32| if d >= 99 { d - 99 } else { d })
            .unwrap_or(0);

        let payload = serde_json::json!({
            "url": opts.url,
            "wait_for": opts.wait_for,
            "selector": opts.selector,
            "format": opts.format,
            "timeout_ms": opts.timeout_ms,
            "user_data_dir": opts.user_data_dir,
            "hydrated_cookies": opts.hydrated_cookies,
            "display": opts.display,
            "screen": screen_num,
            "allowed_origins": opts.allowed_origins,
        });

        let payload =
            serde_json::to_vec(&payload).context("failed to serialize page_text payload")?;
        let out = self
            .exec_input(target, &browser_helper_command(PAGE_TEXT_SCRIPT), &payload)
            .await?;

        parse_page_text_exec_output(&out)
    }

    /// Execute an opaque ref action against its captured native page target.
    pub async fn page_action(&self, target: &str, opts: &PageActionOptions) -> Result<String> {
        let payload = serde_json::json!({
            "target_id": opts.target_id,
            "loader_id": opts.loader_id,
            "selector": opts.selector,
            "backend_node_id": opts.backend_node_id,
            "action": opts.action,
            "button": opts.button,
            "text": opts.text,
            "clear": opts.clear,
            "submit": opts.submit,
            "timeout_ms": opts.timeout_ms,
            "user_data_dir": opts.user_data_dir,
            "display": opts.display,
            "screen": opts.screen,
        });
        let payload =
            serde_json::to_vec(&payload).context("failed to serialize page action payload")?;
        let deadline = Duration::from_millis(opts.timeout_ms.max(1_000).saturating_add(3_000));
        let out = tokio::time::timeout(
            deadline,
            self.exec_input(
                target,
                &browser_helper_command(PAGE_ACTION_SCRIPT),
                &payload,
            ),
        )
        .await
        .context("native page action timed out")??;
        if out.exit_code != 0 {
            bail!("native page action helper failed");
        }
        let line = last_json_line(&out.stdout).context("native page action returned no outcome")?;
        let result: serde_json::Value =
            serde_json::from_str(&line).context("native page action returned malformed output")?;
        if result.get("status").and_then(|v| v.as_str()) != Some("ok") {
            bail!("native page action rejected");
        }
        Ok(line)
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
            "storage_state": opts.storage_state,
            "reason": opts.reason,
            "display": opts.display,
            "headless": false,
        });

        let payload =
            serde_json::to_vec(&payload).context("failed to serialize auth_handoff payload")?;
        let out = self
            .exec_input(
                target,
                &browser_helper_command(AUTH_HANDOFF_SCRIPT),
                &payload,
            )
            .await?;

        parse_auth_handoff_exec_output(&out)
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
    pub format: Option<String>,
    pub timeout_ms: u64,
    /// Persistent Chrome user data dir inside the container.
    pub user_data_dir: Option<String>,
    pub display: Option<String>,
    pub hydrated_cookies: Option<Vec<crate::profile::Cookie>>,
    /// Exact normalized origins allowed for account-scoped observations.
    pub allowed_origins: Option<Vec<String>>,
}

/// Parsed output from the embedded `page_text` Python helper.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct PageTextOutput {
    pub status: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub axtree: Option<String>,
    #[serde(default)]
    pub refs: Option<std::collections::HashMap<String, crate::refs::ElementRef>>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// Native CDP target identity captured once for this snapshot.
    #[serde(default)]
    pub page_target_id: Option<String>,
    /// Native main-frame loader identity captured once for this snapshot.
    #[serde(default)]
    pub page_loader_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub cookies: Vec<crate::profile::Cookie>,
}

/// Inputs to the fixed native Playwright ref-action helper.
#[derive(Debug, Clone)]
pub struct PageActionOptions {
    pub target_id: String,
    pub loader_id: String,
    pub selector: String,
    pub backend_node_id: i64,
    pub action: String,
    pub button: String,
    pub text: String,
    pub clear: bool,
    pub submit: bool,
    pub timeout_ms: u64,
    pub user_data_dir: String,
    pub display: String,
    pub screen: u32,
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
    pub storage_state: Option<String>,
    pub reason: Option<String>,
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
fn parse_page_text_exec_output(out: &ExecOutput) -> Result<PageTextOutput> {
    let parsed = parse_page_text_json(&out.stdout);
    if let Some(parsed) = parsed {
        if parsed.status == "error" {
            bail!(
                "page_text helper failed: {}",
                parsed.message.as_deref().unwrap_or("unknown error")
            );
        }
        if out.exit_code == 0 {
            return Ok(parsed);
        }
    }
    if out.exit_code != 0 {
        bail!(
            "page_text exec failed (exit {}): {}",
            out.exit_code,
            out.stderr
        );
    }
    bail!("page_text returned malformed output: {}", out.stdout.trim());
}

fn parse_auth_handoff_exec_output(out: &ExecOutput) -> Result<AuthHandoffOutput> {
    if let Some(parsed) = parse_auth_handoff_json(&out.stdout) {
        if parsed.status == "error" {
            bail!(
                "auth_handoff helper failed: {}",
                parsed.message.as_deref().unwrap_or("unknown error")
            );
        }
        if out.exit_code == 0 {
            return Ok(parsed);
        }
    }
    if out.exit_code != 0 {
        bail!(
            "auth_handoff exec failed (exit {}): {}",
            out.exit_code,
            out.stderr
        );
    }
    bail!(
        "auth_handoff returned malformed output: {}",
        out.stdout.trim()
    );
}
/// Build the fixed command used by browser helpers. All request data,
/// including URLs and account state, is attached through stdin by callers.
fn browser_helper_command(script: &str) -> Vec<String> {
    vec!["python3".into(), "-c".into(), script.into()]
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
/// Reads its JSON payload from stdin and prints a single-line JSON object on
/// stdout. Request data is never exposed through process arguments or env.
pub const PAGE_TEXT_SCRIPT: &str = concat!(
    include_str!("../assets/browser_page.py"),
    r#"
import json
import sys

try:
    payload = json.load(sys.stdin)
except Exception as exc:
    print(json.dumps({"status": "error", "message": f"invalid stdin payload: {exc}"}))
    sys.exit(0)

url = payload.get("url")
wait_for = payload.get("wait_for")
selector = payload.get("selector")
format_mode = payload.get("format") or "both"
timeout_ms = int(payload.get("timeout_ms") or 30000)
user_data_dir = payload.get("user_data_dir")
hydrated_cookies = payload.get("hydrated_cookies") or []
display = payload.get("display")
screen_id = int(payload.get("screen") or 0)
allowed_origins = payload.get("allowed_origins")
if allowed_origins is not None and not isinstance(allowed_origins, list):
    raise RuntimeError("invalid allowed origin authority")



try:
    from playwright.sync_api import sync_playwright
except Exception as exc:  # pragma: no cover
    print(json.dumps({"status": "error", "message": f"playwright import failed: {exc}"}))
    sys.exit(0)

if display:
    os.environ["DISPLAY"] = display
else:
    os.environ.setdefault("DISPLAY", ":99")


def add_hydrated_cookies(context, cookies):
    if not cookies:
        return
    import time
    now = time.time()
    normalized = []
    for cookie in cookies:
        item = dict(cookie)
        if item.get("expires", -1) <= 0:
            item["expires"] = int(now + 86400 * 30)
        normalized.append(item)
    context.add_cookies(normalized)


owner = None
try:
    with sync_playwright() as p:
        cdp_port = 9222 + screen_id
        connected_cdp = False
        ctx = None
        page = None
        owner = None

        # Probe connecting over CDP to an existing headed browser session (e.g. from browse)
        for attempt in range(3):
            try:
                browser = p.chromium.connect_over_cdp(f"http://127.0.0.1:{cdp_port}", timeout=3000)
                owner = browser
                verify_cdp_profile(browser, user_data_dir)
                if len(browser.contexts) != 1:
                    raise RuntimeError("expected exactly one browser context")
                ctx = browser.contexts[0]
                if ctx.pages:
                    page = current_page(browser, require_focus=True)
                else:
                    if not url:
                        raise RuntimeError("no live browser page to observe")
                    page = ctx.new_page()
                add_hydrated_cookies(ctx, hydrated_cookies)
                connected_cdp = True
                break
            except Exception as exc:
                if str(exc).startswith("profile affinity check failed:"):
                    raise
                if attempt < 2:
                    import time
                    time.sleep(0.3)

        if not connected_cdp:
            if not url:
                raise RuntimeError("no live browser page to observe")
            if user_data_dir:
                os.makedirs(user_data_dir, exist_ok=True)
                ctx = p.chromium.launch_persistent_context(
                    user_data_dir=user_data_dir,
                    headless=False,
                    args=["--no-sandbox", "--disable-gpu", "--no-first-run"],
                )
                owner = ctx
                add_hydrated_cookies(ctx, hydrated_cookies)
                page = ctx.new_page() if not ctx.pages else ctx.pages[0]
            else:
                browser = p.chromium.launch(
                    headless=False,
                    args=["--no-sandbox", "--disable-gpu", "--no-first-run"],
                )
                ctx = browser.new_context()
                add_hydrated_cookies(ctx, hydrated_cookies)
                owner = browser
                page = ctx.new_page()

        navigation_guard = NavigationGuard(page, allowed_origins)
        try:
            if not url:
                pass
            elif connected_cdp and page.url == url:
                try:
                    page.wait_for_load_state("domcontentloaded", timeout=min(5000, timeout_ms))
                except Exception:
                    pass
            else:
                try:
                    page.goto(url, timeout=timeout_ms, wait_until="domcontentloaded")
                except Exception:
                    try:
                        page.wait_for_load_state("domcontentloaded", timeout=min(5000, timeout_ms))
                    except Exception:
                        pass
            if wait_for:
                page.wait_for_selector(wait_for, timeout=timeout_ms)
            else:
                try:
                    page.wait_for_load_state("networkidle", timeout=timeout_ms)
                except Exception:
                    pass
            # Validate the final focused-page origin before any DOM, AX, or cookie extraction.
            navigation_guard.check()

            if selector:
                el = page.query_selector(selector)
                text = el.inner_text() if el else ""
            else:
                text = page.locator("body").inner_text()

            # AXTree semantic reference extraction
            ax_script = """
            (() => {
                const sel = 'a[href], button, input:not([type="hidden"]), select, textarea, [role="button"], [role="link"], [role="checkbox"], [role="radio"], [role="tab"], [role="menuitem"], [role="option"], [role="textbox"], [role="combobox"], [role="searchbox"], [role="heading"], h1, h2, h3, h4, h5, h6, [onclick], [tabindex]:not([tabindex="-1"])';
                const elements = Array.from(document.querySelectorAll(sel));
                const refs = {};
                const treeLines = [];
                let counter = 1;
                const sensitivePattern = /(password|passwd|passcode|one[- ]?time|otp|token|secret|api[- ]?key|credential|card[- ]?number|cvv|cvc|security[- ]?code)/i;

                for (const el of elements) {
                    try {
                        const rect = el.getBoundingClientRect();
                        const style = window.getComputedStyle(el);
                        if (style.display === 'none' || style.visibility === 'hidden' || style.opacity === '0') continue;
                        if (rect.width <= 0 && rect.height <= 0) continue;

                        const tag = el.tagName.toLowerCase();
                        let role = el.getAttribute('role') || '';
                        if (!role) {
                            if (tag === 'input') {
                                role = el.type || 'text';
                            } else if (tag === 'a') {
                                role = 'link';
                            } else if (/^h[1-6]$/.test(tag)) {
                                role = 'heading';
                            } else {
                                role = tag;
                            }
                        }

                        const metadata = [
                            el.getAttribute('type') || '',
                            el.getAttribute('autocomplete') || '',
                            el.getAttribute('name') || '',
                            el.getAttribute('id') || '',
                            el.getAttribute('aria-label') || '',
                            el.getAttribute('placeholder') || '',
                            el.getAttribute('title') || ''
                        ].join(' ');
                        const sensitive = el.hasAttribute('data-reach-sensitive')
                            || role.toLowerCase() === 'password'
                            || (tag === 'input' && (el.type || '').toLowerCase() === 'password')
                            || sensitivePattern.test(metadata);
                        let name = el.getAttribute('aria-label')
                            || el.getAttribute('placeholder')
                            || el.getAttribute('title')
                            || (!sensitive ? (el.innerText || '').slice(0, 100) : '')
                            || (!sensitive ? (el.value || '').slice(0, 100) : '')
                            || '';
                        name = name.replace(/\\s+/g, ' ').trim();

                        const isHeading = role === 'heading';
                        const isInteractive = !isHeading;

                        const cx = Math.round(rect.left + rect.width / 2) + 0;
                        const cy = Math.round(rect.top + rect.height / 2) + 0;
                        const x = Math.round(rect.left) + 0;
                        const y = Math.round(rect.top) + 0;
                        const w = Math.round(rect.width) + 0;
                        const h = Math.round(rect.height) + 0;

                        let refKey = null;
                        if (isInteractive) {
                            refKey = 'e' + counter++;
                            el.setAttribute('data-reach-ref', refKey);
                            refs[refKey] = {
                                ref: refKey,
                                role: role,
                                name: name,
                                value: sensitive ? null : (el.value || null),
                                selector: '[data-reach-ref="' + refKey + '"]',
                                point: [cx, cy],
                                box_bounds: [x, y, w, h],
                                focused: document.activeElement === el,
                                disabled: Boolean(el.disabled || el.getAttribute('aria-disabled') === 'true')
                            };
                        }
                        let flags = [];
                        if (document.activeElement === el) flags.push('focused');
                        if (el.disabled || el.getAttribute('aria-disabled') === 'true') flags.push('disabled');
                        if (sensitive) flags.push('protected');
                        const flagStr = flags.length ? ' (' + flags.join(', ') + ')' : '';
                        const valStr = !sensitive && el.value && el.value !== name ? ' value="' + el.value.slice(0, 50) + '"' : '';

                        if (isHeading) {
                            treeLines.push('[heading "' + name + '"]');
                        } else {
                            treeLines.push('[@' + refKey + ': ' + role + ' "' + name + '"' + valStr + flagStr + ' x=' + x + ' y=' + y + ' w=' + w + ' h=' + h + ']');
                        }
                    } catch (e) {}
                }

                return {
                    refs: refs,
                    axtree: treeLines.join('\\n')
                };
            })()
            """
            ax_data = {"refs": {}, "axtree": ""}
            try:
                ax_data = page.evaluate(ax_script) or {"refs": {}, "axtree": ""}
            except Exception:
                pass

            refs_out = ax_data.get("refs", {})
            axtree_out = ax_data.get("axtree", "")
            native_page_loader_id = page_loader_id(page)
            if not isinstance(refs_out, dict):
                raise RuntimeError("page refs are malformed")
            for ref in refs_out.values():
                if not isinstance(ref, dict) or not ref.get("selector"):
                    raise RuntimeError("page ref is malformed")
                ref["backend_node_id"] = page_backend_node_id(page, ref["selector"])


            try:
                cookies_out = ctx.cookies()
            except Exception:
                cookies_out = []

            native_page_target_id = page_target_id(page)
            result = {
                "status": "ok",
                "url": page.url,
                "title": page.title(),
                "page_target_id": native_page_target_id,
                "page_loader_id": native_page_loader_id,
                "text": text,
                "axtree": axtree_out,
                "refs": refs_out,
                "cookies": cookies_out,
            }
        finally:
            navigation_guard.close()
            try:
                owner.close()
            except Exception:
                pass
except Exception as exc:
    try:
        if owner:
            owner.close()
    except Exception:
        pass
    result = {"status": "error", "message": str(exc)}

print(json.dumps(result))
"#
);

/// Embedded Playwright helper for opaque ref actions. It binds the action to
/// the snapshot's native page target and the currently focused live page.
pub const PAGE_ACTION_SCRIPT: &str = concat!(
    include_str!("../assets/browser_page.py"),
    r#"
import json
import os
import sys

try:
    payload = json.load(sys.stdin)
    target_id = payload["target_id"]
    loader_id = payload["loader_id"]
    backend_node_id = int(payload["backend_node_id"])
    selector = payload["selector"]
    action = payload["action"]
    button = payload.get("button") or "left"
    text = payload.get("text") or ""
    clear = bool(payload.get("clear"))
    submit = bool(payload.get("submit"))
    timeout_ms = max(1, min(int(payload.get("timeout_ms") or 15000), 60000))
    user_data_dir = payload.get("user_data_dir")
    display = payload.get("display")
    screen_id = int(payload.get("screen") or 0)
except Exception:
    print(json.dumps({"status": "error", "message": "invalid action payload"}))
    raise SystemExit(0)

if display:
    os.environ["DISPLAY"] = display
else:
    os.environ.setdefault("DISPLAY", ":99")

try:
    from playwright.sync_api import sync_playwright
except Exception:
    print(json.dumps({"status": "error", "message": "playwright unavailable"}))
    raise SystemExit(0)

try:
    with sync_playwright() as p:
        browser = p.chromium.connect_over_cdp(
            "http://127.0.0.1:%d" % (9222 + screen_id), timeout=3000
        )
        verify_cdp_profile(browser, user_data_dir)
        page = current_page(browser, require_focus=True)
        if page_target_id(page) != target_id:
            raise RuntimeError("stale page target")
        if page_loader_id(page) != loader_id:
            raise RuntimeError("stale page document")
        if page_backend_node_id(page, selector) != backend_node_id:
            raise RuntimeError("stale ref target")
        locator = page.locator(selector)
        if locator.count() != 1:
            raise RuntimeError("ref target is missing or ambiguous")
        if not locator.is_visible() or locator.is_disabled():
            raise RuntimeError("ref target is not actionable")
        if action == "type" and not locator.is_editable():
            raise RuntimeError("ref target is not editable")
        if action == "click":
            locator.click(button=button, timeout=timeout_ms)
        elif action == "type":
            locator.click(timeout=timeout_ms)
            if clear:
                locator.fill("", timeout=timeout_ms)
            if text:
                locator.press_sequentially(text, timeout=timeout_ms)
            if submit:
                page.keyboard.press("Enter")
        else:
            raise RuntimeError("unsupported ref action")
        print(json.dumps({"status": "ok"}))
except Exception as exc:
    safe = {
        "stale page target",
        "stale page document",
        "stale ref target",
        "ref target is missing or ambiguous",
        "ref target is not actionable",
        "ref target is not editable",
        "unsupported ref action",
    }
    message = str(exc) if str(exc) in safe else "native action failed"
    print(json.dumps({"status": "error", "message": message}))

"#
);
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

try:
    payload = json.load(sys.stdin)
except Exception as exc:
    print(json.dumps({"status": "error", "message": f"invalid stdin payload: {exc}"}))
    sys.exit(0)

url = payload.get("url")
wait_for_selector = payload.get("wait_for_selector")
wait_for_url_contains = payload.get("wait_for_url_contains")
timeout_seconds = int(payload.get("timeout_seconds") or 300)
user_data_dir = payload.get("user_data_dir")
display = payload.get("display")

if not user_data_dir:
    print(json.dumps({"status": "error", "message": "missing account-scoped user_data_dir"}))
    sys.exit(0)

if not url:
    print(json.dumps({"status": "error", "message": "missing url"}))
    sys.exit(0)

try:
    from playwright.sync_api import sync_playwright
except Exception as exc:  # pragma: no cover
    print(json.dumps({"status": "error", "message": f"playwright import failed: {exc}"}))
    sys.exit(0)

if display:
    os.environ["DISPLAY"] = display
else:
    os.environ.setdefault("DISPLAY", ":99")
os.makedirs(user_data_dir, exist_ok=True)

needs_wait = bool(wait_for_selector or wait_for_url_contains)

def add_hydrated_cookies(context, cookies):
    if not cookies:
        return
    now = time.time()
    normalized = []
    for cookie in cookies:
        item = dict(cookie)
        if item.get("expires", -1) <= 0:
            item["expires"] = int(now + 86400 * 30)
        normalized.append(item)
    context.add_cookies(normalized)

def install_storage_origins(context, origins):
    if origins is None:
        raise RuntimeError("storage_state origins must be a list")
    if not isinstance(origins, list):
        raise RuntimeError("storage_state origins must be a list")
    if not origins:
        return
    for state in origins:
        if not isinstance(state, dict) or not isinstance(state.get("origin"), str):
            raise RuntimeError("storage_state origin entry is invalid")
        items = state.get("localStorage", [])
        if not isinstance(items, list):
            raise RuntimeError("storage_state localStorage must be a list")
        for item in items:
            if (
                not isinstance(item, dict)
                or not isinstance(item.get("name"), str)
                or not isinstance(item.get("value"), str)
            ):
                raise RuntimeError("storage_state localStorage entry is invalid")
    storage_json = json.dumps(origins)
    context.add_init_script(script="""
const reachStorageOrigins = __REACH_STORAGE_ORIGINS__;
for (const state of reachStorageOrigins) {
  if (state && state.origin && Array.isArray(state.localStorage) &&
      window.location.origin === state.origin) {
    for (const item of state.localStorage) {
      if (item && typeof item.name === "string" && typeof item.value === "string") {
        window.localStorage.setItem(item.name, item.value);
      }
    }
  }
}
""".replace("__REACH_STORAGE_ORIGINS__", storage_json))

ctx = None
try:
    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            user_data_dir=user_data_dir,
            headless=False,
            args=["--no-sandbox", "--disable-gpu", "--no-first-run"],
        )
        storage_state_arg = payload.get("storage_state")
        if storage_state_arg is not None:
            state_data = (
                json.loads(storage_state_arg)
                if isinstance(storage_state_arg, str)
                else storage_state_arg
            )
            if not isinstance(state_data, dict):
                raise RuntimeError("storage_state must be a JSON object")
            cookies = state_data.get("cookies", [])
            if not isinstance(cookies, list):
                raise RuntimeError("storage_state cookies must be a list")
            add_hydrated_cookies(ctx, cookies)
            install_storage_origins(ctx, state_data.get("origins", []))

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


        result = {
            "status": "authenticated" if matched else "timeout",
            "url": page.url,
            "message": None if matched else "auth signal not seen before timeout",
        }
        ctx.close()
except Exception as exc:
    try:
        if ctx:
            ctx.close()
    except Exception:
        pass
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

    #[test]
    fn browser_helper_command_is_fixed_and_payload_free() {
        let command = browser_helper_command("helper");
        assert_eq!(
            command,
            vec![
                "python3".to_string(),
                "-c".to_string(),
                "helper".to_string()
            ]
        );
    }

    #[test]
    fn browser_helper_errors_are_not_successful_results() {
        let out = ExecOutput {
            exit_code: 0,
            stdout: r#"{"status":"error","message":"cookie hydration failed"}"#.into(),
            stderr: String::new(),
        };
        assert!(parse_page_text_exec_output(&out).is_err());

        let out = ExecOutput {
            exit_code: 0,
            stdout: r#"{"status":"error","message":"storage_state invalid"}"#.into(),
            stderr: String::new(),
        };
        assert!(parse_auth_handoff_exec_output(&out).is_err());
    }
}
