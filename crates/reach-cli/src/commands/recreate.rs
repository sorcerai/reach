use clap::Args;
use colored::Colorize;
use reach_cli::docker::DockerClient;
use std::time::Duration;

#[derive(Args)]
pub struct RecreateArgs {
    /// Sandbox name or container ID
    pub target: String,

    /// Replace the image (e.g. after `make lab-load`)
    #[arg(long)]
    pub image: Option<String>,
}

pub async fn run(args: RecreateArgs) -> anyhow::Result<()> {
    let docker = DockerClient::new()?;
    let sandbox = docker.recreate(&args.target, args.image).await?;
    docker
        .wait_healthy(&sandbox.name, Duration::from_secs(45))
        .await?;
    println!(
        "{} recreated {} ({})",
        "\u{2713}".green(),
        sandbox.name,
        &sandbox.container_id[..12]
    );
    Ok(())
}
