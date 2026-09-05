use clap::{Args, Subcommand};
use colored::Colorize;
use reach_cli::config::ReachConfig;
use reach_cli::docker::DockerClient;
use reach_cli::tools::{ToolContext, dispatch};
use reach_cli::vault::{self, normalize_domain};

#[derive(Args, Debug, Clone)]
pub struct VaultArgs {
    #[command(subcommand)]
    pub command: VaultCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum VaultCommand {
    /// Store credentials for a domain in the vault
    Set(SetArgs),
    /// Retrieve stored credentials for a domain
    Get(GetArgs),
    /// List all domains stored in the vault
    List,
    /// Delete credentials for a domain from the vault
    Delete(DeleteArgs),
    /// Generate current 6-digit TOTP code for a domain
    Totp(TotpArgs),
    /// Synthetically type domain credentials (user, pass, totp) into a sandbox
    Inject(InjectArgs),
}

#[derive(Args, Debug, Clone)]
pub struct SetArgs {
    /// Domain to store credentials for (e.g. github.com or https://github.com/login)
    pub domain: String,

    /// Username or email
    #[arg(long, short = 'u')]
    pub user: String,

    /// Password
    #[arg(long, short = 'p')]
    pub pass: String,

    /// Optional Base32 TOTP secret
    #[arg(long, short = 't')]
    pub totp: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct GetArgs {
    /// Domain to retrieve
    pub domain: String,

    /// Reveal plaintext password and TOTP secret (masked by default)
    #[arg(long)]
    pub reveal: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DeleteArgs {
    /// Domain to delete
    pub domain: String,
}

#[derive(Args, Debug, Clone)]
pub struct TotpArgs {
    /// Domain to generate TOTP for
    pub domain: String,
}

#[derive(Args, Debug, Clone)]
pub struct InjectArgs {
    /// Domain whose credentials should be typed
    pub domain: String,

    /// Target screen index (default: 0)
    #[arg(long, default_value = "0")]
    pub screen: u32,

    /// Sandbox container name or ID (auto-detected if omitted)
    #[arg(long)]
    pub target: Option<String>,

    /// Skip checking active window title before injecting credentials
    #[arg(long)]
    pub no_verify_url: bool,
}

pub async fn run(args: VaultArgs) -> anyhow::Result<()> {
    match args.command {
        VaultCommand::Set(set_args) => run_set(set_args),
        VaultCommand::Get(get_args) => run_get(get_args),
        VaultCommand::List => run_list(),
        VaultCommand::Delete(del_args) => run_delete(del_args),
        VaultCommand::Totp(totp_args) => run_totp(totp_args),
        VaultCommand::Inject(inject_args) => run_inject(inject_args).await,
    }
}

pub fn run_set(args: SetArgs) -> anyhow::Result<()> {
    let norm = normalize_domain(&args.domain);
    anyhow::ensure!(!norm.is_empty(), "domain cannot be empty");

    vault::set(&norm, &args.user, &args.pass, args.totp.as_deref())?;
    println!(
        "{} Stored credentials for domain '{}'",
        "\u{2713}".green(),
        norm.bold()
    );
    Ok(())
}

pub fn run_get(args: GetArgs) -> anyhow::Result<()> {
    let norm = normalize_domain(&args.domain);
    let cred = vault::get(&norm).ok_or_else(|| {
        anyhow::anyhow!(
            "credentials for domain '{}' not found in vault",
            args.domain
        )
    })?;

    println!("{:<12} {}", "Domain:".bold(), norm.cyan());
    println!("{:<12} {}", "Username:".bold(), cred.username);
    if args.reveal {
        println!("{:<12} {}", "Password:".bold(), cred.password);
    } else {
        println!(
            "{:<12} {}",
            "Password:".bold(),
            "******** (use --reveal to display)".dimmed()
        );
    }
    if let Some(ref totp) = cred.totp_secret {
        if args.reveal {
            println!("{:<12} {}", "TOTP Secret:".bold(), totp);
        } else {
            println!(
                "{:<12} {}",
                "TOTP Secret:".bold(),
                "******** (use --reveal to display)".dimmed()
            );
        }
        if let Ok(code) = vault::totp_now(totp) {
            println!("{:<12} {}", "Current TOTP:".bold(), code.green().bold());
        }
    } else {
        println!("{:<12} {}", "TOTP:".bold(), "[none]".dimmed());
    }

    Ok(())
}

pub fn run_list() -> anyhow::Result<()> {
    let list = vault::list();
    if list.is_empty() {
        println!("{}", "No credentials stored in vault.".dimmed());
        return Ok(());
    }

    println!(
        "{:<32} {:<24} {:<8}",
        "DOMAIN".bold().cyan(),
        "USERNAME".bold().cyan(),
        "TOTP".bold().cyan(),
    );

    for item in list {
        let totp_status = if item.has_totp {
            "yes".green()
        } else {
            "no".dimmed()
        };
        println!(
            "{:<32} {:<24} {:<8}",
            item.domain, item.username, totp_status
        );
    }

    Ok(())
}

pub fn run_delete(args: DeleteArgs) -> anyhow::Result<()> {
    let norm = normalize_domain(&args.domain);
    let deleted = vault::delete(&norm);
    if deleted {
        println!(
            "{} Deleted credentials for domain '{}'",
            "\u{2713}".green(),
            norm.bold()
        );
        Ok(())
    } else {
        anyhow::bail!(
            "credentials for domain '{}' not found in vault",
            args.domain
        );
    }
}

pub fn run_totp(args: TotpArgs) -> anyhow::Result<()> {
    let code = vault::generate_totp(&args.domain)?;
    println!("{code}");
    Ok(())
}

pub async fn run_inject(args: InjectArgs) -> anyhow::Result<()> {
    let norm = normalize_domain(&args.domain);
    let cred = vault::get(&norm).ok_or_else(|| {
        anyhow::anyhow!(
            "credentials for domain '{}' not found in vault",
            args.domain
        )
    })?;

    let cfg = ReachConfig::load();
    let docker = DockerClient::new(cfg.docker.socket_path())?;

    let target = match args.target {
        Some(t) => t,
        None => {
            let sandboxes = docker.list().await?;
            if sandboxes.is_empty() {
                anyhow::bail!("no running reach sandbox found; specify --target <container>");
            }
            sandboxes[0].name.clone()
        }
    };

    let ctx = ToolContext {
        docker: &docker,
        public_host: cfg.server.effective_public_host(),
        agent: None,
        profile_broker: None,
        cookie_jars: None,
        owner: None,
    };

    tracing::info!(domain = %norm, target = %target, screen = args.screen, "injecting credentials");

    // 0. Verify active window title matches target domain
    if !args.no_verify_url {
        let display = reach_cli::tools::display_for(args.screen);
        let check_cmd =
            format!("DISPLAY={display} xdotool getactivewindow getwindowname 2>/dev/null || true");
        if let Ok(out) = docker
            .exec(&target, &["bash".into(), "-c".into(), check_cmd])
            .await
        {
            let win_title = out.stdout.trim().to_lowercase();
            let domain_stem = norm.split('.').next().unwrap_or(&norm);
            if !win_title.is_empty()
                && !win_title.contains(&norm)
                && !win_title.contains(domain_stem)
            {
                anyhow::bail!(
                    "active window title '{}' does not match target domain '{}'. Aborting credential injection to prevent credential leakage. (Pass --no-verify-url to bypass)",
                    out.stdout.trim(),
                    norm
                );
            }
        }
    }

    // 1. Type username
    let resp = dispatch(
        &ctx,
        "type",
        &serde_json::json!({
            "text": cred.username,
            "screen": args.screen,
        }),
        &target,
    )
    .await;
    if resp.is_error {
        anyhow::bail!("failed to type username: {:?}", resp.content);
    }

    // 2. Press Tab
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let resp = dispatch(
        &ctx,
        "key",
        &serde_json::json!({
            "combo": "Tab",
            "screen": args.screen,
        }),
        &target,
    )
    .await;
    if resp.is_error {
        anyhow::bail!("failed to press Tab: {:?}", resp.content);
    }

    // 3. Type password
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let resp = dispatch(
        &ctx,
        "type",
        &serde_json::json!({
            "text": cred.password,
            "screen": args.screen,
        }),
        &target,
    )
    .await;
    if resp.is_error {
        anyhow::bail!("failed to type password: {:?}", resp.content);
    }

    // 4. Submit password & handle TOTP 2FA if configured
    if let Some(ref totp_secret) = cred.totp_secret {
        // Submit username & password
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = dispatch(
            &ctx,
            "key",
            &serde_json::json!({
                "combo": "Return",
                "screen": args.screen,
            }),
            &target,
        )
        .await;

        // Wait for 2FA screen to render
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        let code = vault::totp_now(totp_secret)?;
        let resp = dispatch(
            &ctx,
            "type",
            &serde_json::json!({
                "text": code,
                "screen": args.screen,
            }),
            &target,
        )
        .await;
        if resp.is_error {
            anyhow::bail!("failed to type TOTP: {:?}", resp.content);
        }

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = dispatch(
            &ctx,
            "key",
            &serde_json::json!({
                "combo": "Return",
                "screen": args.screen,
            }),
            &target,
        )
        .await;
    }

    println!(
        "{} Injected credentials for '{}' into {} (screen {})",
        "\u{2713}".green(),
        norm.bold(),
        target.bold(),
        args.screen
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct TestCli {
        #[command(subcommand)]
        vault: VaultCli,
    }

    #[derive(Subcommand, Debug)]
    enum VaultCli {
        Vault(VaultArgs),
    }

    #[test]
    fn parses_vault_set_full() {
        let cli = TestCli::parse_from([
            "reach",
            "vault",
            "set",
            "github.com",
            "--user",
            "octocat",
            "--pass",
            "secretpass",
            "--totp",
            "JBSWY3DPEHPK3PXP",
        ]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::Set(args),
            }) => {
                assert_eq!(args.domain, "github.com");
                assert_eq!(args.user, "octocat");
                assert_eq!(args.pass, "secretpass");
                assert_eq!(args.totp.as_deref(), Some("JBSWY3DPEHPK3PXP"));
            }
            _ => panic!("expected Set command"),
        }
    }

    #[test]
    fn parses_vault_set_short_flags() {
        let cli = TestCli::parse_from([
            "agent-computer",
            "vault",
            "set",
            "https://google.com/login",
            "-u",
            "user@gmail.com",
            "-p",
            "mypass",
            "-t",
            "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
        ]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::Set(args),
            }) => {
                assert_eq!(args.domain, "https://google.com/login");
                assert_eq!(args.user, "user@gmail.com");
                assert_eq!(args.pass, "mypass");
                assert_eq!(
                    args.totp.as_deref(),
                    Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ")
                );
            }
            _ => panic!("expected Set command"),
        }
    }

    #[test]
    fn parses_vault_get() {
        let cli = TestCli::parse_from(["reach", "vault", "get", "github.com"]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::Get(args),
            }) => {
                assert_eq!(args.domain, "github.com");
            }
            _ => panic!("expected Get command"),
        }
    }

    #[test]
    fn parses_vault_list() {
        let cli = TestCli::parse_from(["reach", "vault", "list"]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::List,
            }) => {}
            _ => panic!("expected List command"),
        }
    }

    #[test]
    fn parses_vault_delete() {
        let cli = TestCli::parse_from(["reach", "vault", "delete", "github.com"]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::Delete(args),
            }) => {
                assert_eq!(args.domain, "github.com");
            }
            _ => panic!("expected Delete command"),
        }
    }

    #[test]
    fn parses_vault_totp() {
        let cli = TestCli::parse_from(["agent-computer", "vault", "totp", "github.com"]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::Totp(args),
            }) => {
                assert_eq!(args.domain, "github.com");
            }
            _ => panic!("expected Totp command"),
        }
    }

    #[test]
    fn parses_vault_inject() {
        let cli = TestCli::parse_from([
            "agent-computer",
            "vault",
            "inject",
            "github.com",
            "--screen",
            "2",
            "--target",
            "my-sandbox",
        ]);
        match cli.vault {
            VaultCli::Vault(VaultArgs {
                command: VaultCommand::Inject(args),
            }) => {
                assert_eq!(args.domain, "github.com");
                assert_eq!(args.screen, 2);
                assert_eq!(args.target.as_deref(), Some("my-sandbox"));
            }
            _ => panic!("expected Inject command"),
        }
    }

    #[test]
    fn execution_set_get_list_totp_delete() {
        let temp_dir =
            std::env::temp_dir().join(format!("reach_cli_vault_test_{}", uuid::Uuid::new_v4()));
        let file_path = temp_dir.join("secrets.json");
        unsafe {
            std::env::set_var("REACH_VAULT_PATH", &file_path);
        }

        // Set
        let set_res = run_set(SetArgs {
            domain: "https://github.com/login".to_string(),
            user: "octocat".to_string(),
            pass: "secret123".to_string(),
            totp: Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_string()),
        });
        assert!(set_res.is_ok());

        // Get
        let get_res = run_get(GetArgs {
            domain: "github.com".to_string(),
            reveal: false,
        });
        assert!(get_res.is_ok());

        let get_reveal = run_get(GetArgs {
            domain: "github.com".to_string(),
            reveal: true,
        });
        assert!(get_reveal.is_ok());

        // List
        let list_res = run_list();
        assert!(list_res.is_ok());

        // Totp
        let totp_res = run_totp(TotpArgs {
            domain: "github.com".to_string(),
        });
        assert!(totp_res.is_ok());

        // Delete
        let del_res = run_delete(DeleteArgs {
            domain: "github.com".to_string(),
        });
        assert!(del_res.is_ok());

        // Second delete fails
        let del_res2 = run_delete(DeleteArgs {
            domain: "github.com".to_string(),
        });
        assert!(del_res2.is_err());

        unsafe {
            std::env::remove_var("REACH_VAULT_PATH");
        }
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
