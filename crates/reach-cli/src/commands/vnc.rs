use clap::Args;
use colored::Colorize;
use reach_cli::config::ReachConfig;
use reach_cli::docker::DockerClient;

#[derive(Args)]
pub struct VncArgs {
    /// Sandbox name or container ID
    pub target: String,
    /// Authenticated supervisor API for this sandbox
    #[arg(long)]
    pub api_url: Option<String>,
    #[arg(long, default_value = "0")]
    pub screen: u32,
}

pub async fn run(args: VncArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load();
    let docker = DockerClient::new(cfg.docker.socket_path())?;
    let sandbox = docker.find(&args.target).await?;

    let base = url::Url::parse(
        &args
            .api_url
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.server.port)),
    )?;
    let token = std::env::var("REACH_AUTH_TOKEN").map_err(|_| {
        anyhow::anyhow!("REACH_AUTH_TOKEN is required to verify the supervisor's bound computer")
    })?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response: serde_json::Value = client
        .get(base.join("/agent/computer")?)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if response["sandbox"].as_str() != Some(sandbox.name.as_str()) {
        anyhow::bail!("supervisor is bound to another computer; select the correct --api-url");
    }
    if args.screen >= sandbox.ports.screens {
        anyhow::bail!("screen does not exist");
    }
    let url = base.join(&format!("/viewer/{}", args.screen))?.to_string();
    println!("{} {}", "Opening...".dimmed(), url.cyan());
    open::that(&url)?;
    Ok(())
}
