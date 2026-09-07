use clap::Args;
use colored::Colorize;
use reach_cli::config::ReachConfig;
use reach_cli::docker::DockerClient;

use super::create::read_lifecycle_manifest;

#[derive(Args)]
pub struct DestroyArgs {
    /// Sandbox name or container ID
    pub target: String,
}

pub async fn run(args: DestroyArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load();
    let docker = DockerClient::new(cfg.docker.socket_path())?;
    let inspected = docker.inspect_config(&args.target).await?;
    let (manifest_path, _) = read_lifecycle_manifest(&cfg, &inspected)?;
    docker.destroy(&args.target).await?;
    std::fs::remove_file(&manifest_path).map_err(|error| {
        anyhow::anyhow!(
            "sandbox '{}' was destroyed, but lifecycle manifest '{}' could not be removed: {}",
            inspected.name,
            manifest_path.display(),
            error
        )
    })?;
    println!(
        "{} {}",
        "\u{2717}".red(),
        format!("Sandbox \"{}\" destroyed.", args.target).dimmed()
    );
    Ok(())
}
