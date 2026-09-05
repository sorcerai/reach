use clap::Args;
use reach_cli::config::ReachConfig;
use reach_cli::docker::{DockerClient, ProfileMount};
use reach_cli::profile::{CookieJarService, LockHolderInfo, ProfileBroker};
use reach_cli::tools::browse_command_with_hydration;

#[derive(Args, Clone, Debug)]
pub struct BrowseArgs {
    /// URL to open in Chrome
    #[arg(long, default_value = "about:blank")]
    pub url: String,

    /// Domain jars to hydrate (comma-separated or repeated)
    #[arg(long, value_delimiter = ',')]
    pub jars: Vec<String>,

    /// Profile name to use (default: ephemeral if jars specified without profile, else 'default')
    #[arg(long)]
    pub profile: Option<String>,

    /// Launch ephemeral browser session (/tmp/ctx-<uuid>)
    #[arg(long)]
    pub ephemeral: bool,

    /// Target sandbox container
    #[arg(long)]
    pub sandbox: Option<String>,

    /// Screen index (default: 0)
    #[arg(long, default_value = "0")]
    pub screen: u32,

    /// Lock timeout in milliseconds (default: 5000)
    #[arg(long, default_value = "5000")]
    pub timeout_ms: u64,
}

pub async fn run(args: BrowseArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load();
    let docker = DockerClient::new(cfg.docker.socket_path())?;

    let target = match args.sandbox.as_deref() {
        Some(s) => s.to_string(),
        None => {
            let list = docker.list().await?;
            list.into_iter()
                .find(|s| matches!(s.status, reach_cli::docker::SandboxStatus::Running))
                .map(|s| s.name)
                .ok_or_else(|| anyhow::anyhow!("no running sandbox found"))?
        }
    };

    let is_ephemeral = args.ephemeral || (args.profile.is_none() && !args.jars.is_empty());
    let profile_name = if is_ephemeral {
        format!("/tmp/ctx-{}", uuid::Uuid::new_v4())
    } else {
        args.profile.clone().unwrap_or_else(|| {
            if args.screen > 0 {
                format!("default-screen{}", args.screen)
            } else {
                "default".to_string()
            }
        })
    };

    let profile_dir = if is_ephemeral {
        profile_name.clone()
    } else {
        ProfileMount::container_path_for(&profile_name)
    };

    let broker = ProfileBroker::default_broker();
    let holder = LockHolderInfo::new(Some(args.screen), Some("reach browse".into()), None);
    let _lease = broker
        .acquire_with_holder(&profile_name, args.timeout_ms, Some(holder))
        .map_err(|e| anyhow::anyhow!("failed to acquire profile lock for '{profile_name}': {e}"))?;

    let jars_svc = CookieJarService::default_service();
    let hydrated_json = if !args.jars.is_empty() {
        let st = jars_svc.hydrate_jars(&args.jars);
        serde_json::to_string(&st).ok()
    } else {
        None
    };

    let cmd = browse_command_with_hydration(&args.url, &profile_dir, hydrated_json.as_deref());
    let display = format!(":{}", 99 + args.screen);
    docker
        .exec(
            &target,
            &[
                "bash".into(),
                "-c".into(),
                format!("DISPLAY={display} {cmd}"),
            ],
        )
        .await?;

    println!(
        "Opened {} in sandbox '{}' on screen {} (profile: {})",
        args.url, target, args.screen, profile_name
    );

    Ok(())
}
