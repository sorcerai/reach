use clap::Args;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug)]
#[command(name = "drive", about = "Run Reach CUA Driver vision-action loop")]
pub struct DriveArgs {
    /// Goal or objective for the browser / screen
    #[arg(long)]
    pub goal: String,

    /// Target screen index
    #[arg(long, default_value_t = 0)]
    pub screen: u32,

    /// Initial URL to navigate
    #[arg(long)]
    pub initial_url: Option<String>,

    /// Reach Agent API URL
    #[arg(long, default_value = "http://127.0.0.1:4200")]
    pub api_url: String,

    /// Path to python executable
    #[arg(long)]
    pub python_bin: Option<String>,

    /// Path to reach_drive.py script
    #[arg(long)]
    pub script_path: Option<PathBuf>,

    /// Extra trailing arguments passed to reach_drive.py
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub extra_args: Vec<String>,
}

pub async fn run(args: DriveArgs) -> anyhow::Result<()> {
    let script = args.script_path.unwrap_or_else(|| {
        for candidate in &[
            PathBuf::from("scripts/reach_drive.py"),
            PathBuf::from("../scripts/reach_drive.py"),
            PathBuf::from("../../scripts/reach_drive.py"),
            PathBuf::from("/srv/reach/scripts/reach_drive.py"),
        ] {
            if candidate.is_file() {
                return candidate.clone();
            }
        }
        PathBuf::from("scripts/reach_drive.py")
    });

    let python = args
        .python_bin
        .or_else(|| std::env::var("PYTHON_BIN").ok())
        .unwrap_or_else(|| "python3".to_string());

    let api_url = if args.api_url != "http://127.0.0.1:4200" {
        args.api_url
    } else {
        std::env::var("REACH_AGENT_URL").unwrap_or(args.api_url)
    };

    let mut cmd = Command::new(&python);
    cmd.arg(&script);
    cmd.arg("--goal").arg(&args.goal);
    cmd.arg("--screen").arg(args.screen.to_string());
    cmd.arg("--api-url").arg(&api_url);

    if let Some(url) = &args.initial_url {
        cmd.arg("--initial-url").arg(url);
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
