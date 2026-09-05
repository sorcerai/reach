use clap::{Args, Subcommand};
use colored::Colorize;
use reach_cli::agent_card::{AgentCardEngine, CardStatus, DEFAULT_APPROVAL_THRESHOLD_USD};
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct CardArgs {
    #[arg(long, global = true, help = "Custom path to cards.json")]
    pub cards_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: CardSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum CardSubcommand {
    /// Mint a new virtual card bounded to a merchant and spending limit
    Mint(MintArgs),
    /// List virtual cards
    List(ListArgs),
    /// Approve spending on a pending virtual card
    Approve(ApproveArgs),
    /// Lock a virtual card so it cannot be used
    Lock(LockArgs),
    /// Simulate charging a virtual card
    Charge(ChargeArgs),
    /// Inject card details directly into checkout form on a screen
    Inject(InjectArgs),
}

#[derive(Args, Debug)]
pub struct MintArgs {
    #[arg(long, help = "Target merchant domain (e.g. amazon.com)")]
    pub merchant: String,

    #[arg(long, help = "Spending limit in USD")]
    pub limit: f64,

    #[arg(
        long,
        default_value_t = DEFAULT_APPROVAL_THRESHOLD_USD,
        help = "Approval threshold in USD (default 25.00)"
    )]
    pub threshold: f64,

    #[arg(long, default_value = "USD", help = "Currency code")]
    pub currency: String,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long, help = "Filter by merchant domain")]
    pub merchant: Option<String>,

    #[arg(long, help = "Filter by status (PENDING_APPROVAL, ACTIVE, etc.)")]
    pub status: Option<String>,

    #[arg(long, help = "Output as raw JSON")]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ApproveArgs {
    #[arg(help = "Card ID to approve")]
    pub id: String,
}

#[derive(Args, Debug)]
pub struct LockArgs {
    #[arg(help = "Card ID to lock")]
    pub id: String,
}

#[derive(Args, Debug)]
pub struct ChargeArgs {
    #[arg(help = "Card ID to charge")]
    pub id: String,

    #[arg(long, help = "Charge amount in USD")]
    pub amount: f64,

    #[arg(long, help = "Merchant domain")]
    pub merchant: Option<String>,
}

#[derive(Args, Debug)]
pub struct InjectArgs {
    #[arg(help = "Card ID to inject")]
    pub id: String,

    #[arg(long, default_value_t = 0, help = "Target screen ID")]
    pub screen: u32,

    #[arg(long, default_value = "agent-computer", help = "Target container name")]
    pub container: String,

    #[arg(long, help = "Submit form after typing CVV")]
    pub submit: bool,

    #[arg(long, help = "Split expiration into month then Tab then year")]
    pub split_exp: bool,
}

pub async fn run(args: CardArgs) -> anyhow::Result<()> {
    let mut engine = AgentCardEngine::new(args.cards_path);

    match args.command {
        CardSubcommand::Mint(mint_args) => {
            let card = engine.mint_card(
                &mint_args.merchant,
                mint_args.limit,
                mint_args.threshold,
                Some(&mint_args.currency),
                None,
            )?;

            println!("{}", serde_json::to_string_pretty(&card.to_safe_view())?);
        }
        CardSubcommand::List(list_args) => {
            let status_filter =
                list_args
                    .status
                    .as_deref()
                    .map(|s| match s.to_uppercase().as_str() {
                        "PENDING_APPROVAL" => CardStatus::PendingApproval,
                        "ACTIVE" => CardStatus::Active,
                        "CHARGED" => CardStatus::Charged,
                        "LOCKED" => CardStatus::Locked,
                        "EXPIRED" => CardStatus::Expired,
                        _ => CardStatus::Active,
                    });

            let cards = engine.list_cards(list_args.merchant.as_deref(), status_filter)?;

            if list_args.json {
                let views: Vec<_> = cards.iter().map(|c| c.to_safe_view()).collect();
                println!("{}", serde_json::to_string_pretty(&views)?);
            } else if cards.is_empty() {
                println!("{}", "No virtual cards found.".dimmed());
            } else {
                println!(
                    "{:<14} {:<20} {:<10} {:>10} {:<18} {:<10}",
                    "CARD ID".bold().cyan(),
                    "MERCHANT".bold().cyan(),
                    "LIMIT".bold().cyan(),
                    "PAN".bold().cyan(),
                    "STATUS".bold().cyan(),
                    "EXP".bold().cyan(),
                );

                for c in cards {
                    let status_str = format!("{}", c.status);
                    let status_colored = match c.status {
                        CardStatus::Active => status_str.green().to_string(),
                        CardStatus::PendingApproval => status_str.yellow().to_string(),
                        CardStatus::Charged => status_str.blue().to_string(),
                        CardStatus::Locked => status_str.red().to_string(),
                        CardStatus::Expired => status_str.dimmed().to_string(),
                    };

                    println!(
                        "{:<14} {:<20} ${:<9.2} {:>10} {:<18} {}/{}",
                        c.id,
                        c.merchant,
                        c.spending_limit_usd,
                        c.masked_card_number(),
                        status_colored,
                        c.exp_month,
                        c.exp_year,
                    );
                }
            }
        }
        CardSubcommand::Approve(app_args) => {
            let card = engine.approve_card(&app_args.id)?;
            println!("{}", serde_json::to_string_pretty(&card.to_safe_view())?);
        }
        CardSubcommand::Lock(lock_args) => {
            let card = engine.lock_card(&lock_args.id)?;
            println!("{}", serde_json::to_string_pretty(&card.to_safe_view())?);
        }
        CardSubcommand::Charge(charge_args) => {
            let card = engine.charge_card(
                &charge_args.id,
                charge_args.amount,
                charge_args.merchant.as_deref(),
            )?;
            println!("{}", serde_json::to_string_pretty(&card.to_safe_view())?);
        }
        CardSubcommand::Inject(inj_args) => {
            let res = engine.inject_card(
                inj_args.screen,
                &inj_args.id,
                &inj_args.container,
                inj_args.submit,
                inj_args.split_exp,
            )?;
            println!("{}", serde_json::to_string_pretty(&res)?);
        }
    }

    Ok(())
}
