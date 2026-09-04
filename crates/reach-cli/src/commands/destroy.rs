use clap::Args;
use colored::Colorize;
use reach_cli::config::ReachConfig;
use reach_cli::docker::DockerClient;

#[derive(Args)]
pub struct DestroyArgs {
    /// Sandbox name or container ID
    pub target: String,
}

pub async fn run(args: DestroyArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load();
    let docker = DockerClient::new(cfg.docker.socket_path())?;
    docker.destroy(&args.target).await?;
    println!(
        "{} {}",
        "\u{2717}".red(),
        format!("Sandbox \"{}\" destroyed.", args.target).dimmed()
    );
    Ok(())
}
