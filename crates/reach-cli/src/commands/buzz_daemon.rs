use clap::Args;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug)]
#[command(
    name = "buzz-daemon",
    about = "Run the Live Buzz Agent Daemon (@ReachBot continuous listener)"
)]
pub struct BuzzDaemonArgs {
    /// Buzz relay HTTP URL
    #[arg(long, default_value = "http://100.124.38.17:3000")]
    pub relay: String,

    /// Buzz relay WebSocket URL
    #[arg(long, default_value = "ws://100.124.38.17:3000")]
    pub ws_relay: String,

    /// Reach Agent API URL
    #[arg(long, default_value = "http://127.0.0.1:4200")]
    pub api_url: String,

    /// Bot mention trigger string
    #[arg(long, default_value = "@ReachBot")]
    pub trigger: String,

    /// Channel(s) to monitor
    #[arg(short, long)]
    pub channel: Option<String>,

    /// Default screen index
    #[arg(short, long, default_value_t = 0)]
    pub screen: u32,

    /// Polling interval in seconds
    #[arg(long, default_value_t = 2.0)]
    pub poll_interval: f64,

    /// Run once and exit
    #[arg(long)]
    pub once: bool,

    /// Path to python executable
    #[arg(long)]
    pub python_bin: Option<String>,

    /// Path to buzz_daemon.py script
    #[arg(long)]
    pub script_path: Option<PathBuf>,

    /// Extra trailing arguments passed to buzz_daemon.py
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub extra_args: Vec<String>,
}

pub async fn run(args: BuzzDaemonArgs) -> anyhow::Result<()> {
    let script = args.script_path.unwrap_or_else(|| {
        for candidate in &[
            PathBuf::from("scripts/buzz_daemon.py"),
            PathBuf::from("../scripts/buzz_daemon.py"),
            PathBuf::from("../../scripts/buzz_daemon.py"),
            PathBuf::from("/srv/reach/scripts/buzz_daemon.py"),
        ] {
            if candidate.is_file() {
                return candidate.clone();
            }
        }
        PathBuf::from("scripts/buzz_daemon.py")
    });

    let python = args
        .python_bin
        .or_else(|| std::env::var("PYTHON_BIN").ok())
        .unwrap_or_else(|| "python3".to_string());

    let relay = if args.relay != "http://100.124.38.17:3000" {
        args.relay
    } else {
        std::env::var("BUZZ_RELAY_URL").unwrap_or(args.relay)
    };

    let ws_relay = if args.ws_relay != "ws://100.124.38.17:3000" {
        args.ws_relay
    } else {
        std::env::var("BUZZ_WS_RELAY_URL").unwrap_or(args.ws_relay)
    };

    let api_url = if args.api_url != "http://127.0.0.1:4200" {
        args.api_url
    } else {
        std::env::var("REACH_AGENT_URL").unwrap_or(args.api_url)
    };

    let mut cmd = Command::new(&python);
    cmd.arg(&script);
    cmd.arg("--relay").arg(&relay);
    cmd.arg("--ws-relay").arg(&ws_relay);
    cmd.arg("--api-url").arg(&api_url);
    cmd.arg("--trigger").arg(&args.trigger);
    cmd.arg("--screen").arg(args.screen.to_string());
    cmd.arg("--poll-interval")
        .arg(args.poll_interval.to_string());

    if let Some(ch) = &args.channel {
        cmd.arg("--channel").arg(ch);
    }
    if args.once {
        cmd.arg("--once");
    }
    for extra in &args.extra_args {
        cmd.arg(extra);
    }

    let status = cmd.status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
