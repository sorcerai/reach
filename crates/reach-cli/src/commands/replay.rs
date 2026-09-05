use clap::Args;
use colored::Colorize;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug, Clone)]
pub struct ReplayArgs {
    /// Routine name to replay
    #[arg(long)]
    pub routine: String,

    /// Parameters as JSON string, e.g. '{"query": "Tesla"}'
    #[arg(long)]
    pub params: Option<String>,

    /// Screen ID override (default: screen from routine.json)
    #[arg(long)]
    pub screen: Option<u32>,

    /// Optional custom base directory for routines
    #[arg(long)]
    pub routines_dir: Option<PathBuf>,

    /// Reach agent API URL
    #[arg(long, default_value = "http://127.0.0.1:4200")]
    pub api_url: String,

    /// Optional target sandbox container
    #[arg(long)]
    pub sandbox: Option<String>,

    /// Disable CUA vision self-healing fallback
    #[arg(long)]
    pub no_heal: bool,

    /// Output replay result as JSON
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: ReplayArgs) -> anyhow::Result<()> {
    if !args.json {
        println!(
            "{} Replaying routine '{}'...",
            "\u{25b6}".cyan(),
            args.routine.bold()
        );
    }

    let script_path = find_routine_script()?;

    let mut cmd = Command::new("python3");
    cmd.arg(&script_path);
    cmd.arg("replay");
    cmd.arg("--routine").arg(&args.routine);
    cmd.arg("--api-url").arg(&args.api_url);

    if let Some(params) = &args.params {
        cmd.arg("--params").arg(params);
    }
    if let Some(s) = args.screen {
        cmd.arg("--screen").arg(s.to_string());
    }
    if let Some(rd) = &args.routines_dir {
        cmd.arg("--routines-dir").arg(rd);
    }
    if let Some(sb) = &args.sandbox {
        cmd.arg("--sandbox").arg(sb);
    }
    if args.no_heal {
        cmd.arg("--no-heal");
    }
    if args.json {
        cmd.arg("--json");
    }

    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("Replay exited with code {:?}", status.code());
    }

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

    Ok(PathBuf::from("scripts/reach_routine.py"))
}
