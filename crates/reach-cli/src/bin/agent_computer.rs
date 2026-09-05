#[path = "../commands/mod.rs"]
mod commands;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "agent-computer",
    about = "Agent Computer Operating System & Workstation Fleet"
)]
#[command(version, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: commands::Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agent_computer=info".into()),
        )
        .init();

    let cli = Cli::parse();
    commands::run(cli.command).await
}
