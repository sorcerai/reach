use clap::Args;
use colored::Colorize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug, Clone)]
pub struct RecordArgs {
    /// Screen ID to record
    #[arg(long, default_value = "0")]
    pub screen: u32,

    /// Routine name (stored in ~/.reach/routines/<name>/)
    #[arg(long)]
    pub name: String,

    /// Optional initial URL to open for demonstration
    #[arg(long)]
    pub url: Option<String>,

    /// Fallback to manual terminal REPL instead of automated CDP event tap
    #[arg(long)]
    pub manual: bool,

    /// Optional custom base directory for routines
    #[arg(long)]
    pub routines_dir: Option<PathBuf>,

    /// Reach agent API URL
    #[arg(long, default_value = "http://127.0.0.1:4200")]
    pub api_url: String,

    /// Optional target sandbox container
    #[arg(long)]
    pub sandbox: Option<String>,
}

pub async fn run(args: RecordArgs) -> anyhow::Result<()> {
    println!(
        "{} Starting demonstration recorder for routine '{}' on screen :{}",
        "\u{25b6}".cyan(),
        args.name.bold(),
        99 + args.screen
    );

    // Resolve script path
    let script_path = find_routine_script()?;

    let mut cmd = Command::new("python3");
    cmd.arg(&script_path);
    cmd.arg("record");
    cmd.arg("--name").arg(&args.name);
    cmd.arg("--screen").arg(args.screen.to_string());
    cmd.arg("--api-url").arg(&args.api_url);

    if let Some(url) = &args.url {
        cmd.arg("--url").arg(url);
    }
    if args.manual {
        cmd.arg("--manual");
    }
    if let Some(rd) = &args.routines_dir {
        cmd.arg("--routines-dir").arg(rd);
    }
    if let Some(sb) = &args.sandbox {
        cmd.arg("--sandbox").arg(sb);
    }

    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("Recorder process exited with code {:?}", status.code());
    }

    println!(
        "{} Routine '{}' recorded successfully.",
        "\u{2713}".green(),
        args.name.bold()
    );
    Ok(())
}

fn find_routine_script() -> anyhow::Result<PathBuf> {
    let candidates = [
        PathBuf::from("scripts/reach_routine.py"),
        PathBuf::from("../scripts/reach_routine.py"),
        PathBuf::from("../../scripts/reach_routine.py"),
    ];

    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }

    // Check relative to current exe
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let p1 = parent.join("scripts").join("reach_routine.py");
        if p1.is_file() {
            return Ok(p1);
        }
        if let Some(grandparent) = parent.parent() {
            let p2 = grandparent.join("scripts").join("reach_routine.py");
            if p2.is_file() {
                return Ok(p2);
            }
        }
    }

    // Fallback to default
    Ok(PathBuf::from("scripts/reach_routine.py"))
}
