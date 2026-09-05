pub mod browse;
pub mod buzz_daemon;
pub mod card;
pub mod connect;
pub mod create;
pub mod destroy;
pub mod drive;
pub mod exec;
pub mod list;
pub mod record;
pub mod recreate;
pub mod replay;
pub mod screenshot;
pub mod serve;
pub mod vnc;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Command {
    /// Create a new sandbox container
    Create(create::CreateArgs),
    /// Destroy a sandbox container
    Destroy(destroy::DestroyArgs),
    /// Destroy and recreate a sandbox, keeping its workspace and profile volumes
    Recreate(recreate::RecreateArgs),
    /// List running sandbox containers
    List,
    /// Attach MCP stdio bridge to a sandbox
    Connect(connect::ConnectArgs),
    /// Run a command inside a sandbox
    Exec(exec::ExecArgs),
    /// Start MCP SSE server proxying to sandbox(es)
    Serve(serve::ServeArgs),
    /// Open a URL in Chrome with profile broker and cookie jar hydration
    Browse(browse::BrowseArgs),
    /// Open noVNC in browser
    Vnc(vnc::VncArgs),
    /// Capture a screenshot from a sandbox
    Screenshot(screenshot::ScreenshotArgs),
    /// Record an interactive routine demonstration
    Record(record::RecordArgs),
    /// Replay a compiled routine with CUA self-healing
    Replay(replay::ReplayArgs),
    /// Manage virtual agent cards and bounded spending limits
    Card(card::CardArgs),
    /// Run the Live Buzz Agent Daemon (@ReachBot continuous listener)
    BuzzDaemon(buzz_daemon::BuzzDaemonArgs),
    /// Run Reach CUA Driver vision-action loop
    Drive(drive::DriveArgs),
}

pub async fn run(cmd: Command) -> anyhow::Result<()> {
    match cmd {
        Command::Create(args) => create::run(args).await,
        Command::Destroy(args) => destroy::run(args).await,
        Command::Recreate(args) => recreate::run(args).await,
        Command::List => list::run().await,
        Command::Connect(args) => connect::run(args).await,
        Command::Exec(args) => exec::run(args).await,
        Command::Serve(args) => serve::run(args).await,
        Command::Browse(args) => browse::run(args).await,
        Command::Vnc(args) => vnc::run(args).await,
        Command::Screenshot(args) => screenshot::run(args).await,
        Command::Record(args) => record::run(args).await,
        Command::Replay(args) => replay::run(args).await,
        Command::Card(args) => card::run(args).await,
        Command::BuzzDaemon(args) => buzz_daemon::run(args).await,
        Command::Drive(args) => drive::run(args).await,
    }
}
