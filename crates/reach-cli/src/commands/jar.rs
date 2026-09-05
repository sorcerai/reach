use clap::{Args, Subcommand};
use colored::Colorize;
use reach_cli::profile::{Cookie, CookieJarService, StorageState};
use std::fs;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct JarArgs {
    #[arg(long, global = true, help = "Custom path to jars directory")]
    pub jars_path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: JarCommand,
}

#[derive(Subcommand, Debug, Clone)]
pub enum JarCommand {
    /// Import cookies from a Playwright storage_state JSON file into domain-sharded jars
    Import(ImportArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    /// Path to Playwright storage_state JSON file
    pub file: PathBuf,

    /// Target domains to import cookies for (comma-separated or repeated)
    #[arg(long, short = 'd', value_delimiter = ',')]
    pub domains: Option<Vec<String>>,
}

pub async fn run(args: JarArgs) -> anyhow::Result<()> {
    match args.command {
        JarCommand::Import(import_args) => run_import(args.jars_path, import_args).await,
    }
}

pub async fn run_import(custom_jars_path: Option<PathBuf>, args: ImportArgs) -> anyhow::Result<()> {
    let file_path = &args.file;
    if !file_path.exists() {
        anyhow::bail!("Storage state file not found: {:?}", file_path);
    }

    let content = fs::read_to_string(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read storage state file {:?}: {e}", file_path))?;

    if content.trim().is_empty() {
        anyhow::bail!("Storage state file is empty: {:?}", file_path);
    }

    let storage_state: StorageState =
        if let Ok(state) = serde_json::from_str::<StorageState>(&content) {
            state
        } else if let Ok(cookies) = serde_json::from_str::<Vec<Cookie>>(&content) {
            StorageState {
                jar_version: 0,
                cookies,
                origins: Vec::new(),
            }
        } else {
            anyhow::bail!("Failed to parse storage state JSON from {:?}", file_path);
        };

    let jars_svc = match custom_jars_path {
        Some(p) => CookieJarService::new(p),
        None => CookieJarService::default_service(),
    };

    let declared_domains = args.domains.unwrap_or_default();

    let target_domains: Vec<String> = if declared_domains.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for c in &storage_state.cookies {
            let clean = CookieJarService::sanitize_domain(&c.domain);
            if !clean.is_empty() && clean != "default" {
                seen.insert(clean);
            }
        }
        seen.into_iter().collect()
    } else {
        declared_domains
    };

    if target_domains.is_empty() {
        println!("No cookies or target domains found to import.");
        return Ok(());
    }

    // 1. Record pre-import version and verify lock is not currently held
    let mut pre_versions = std::collections::HashMap::new();
    for domain in &target_domains {
        let clean = CookieJarService::sanitize_domain(domain);
        let pre_v = jars_svc
            .load_jar(&clean)
            .map(|s| s.jar_version)
            .unwrap_or(0);
        pre_versions.insert(clean.clone(), pre_v);

        let lock_path = jars_svc.jars_dir().join(format!(".{clean}.lock"));
        if lock_path.exists() {
            let lock_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path);
            if let Ok(f) = lock_file {
                if fs4::FileExt::try_lock(&f).is_err() {
                    anyhow::bail!("Domain jar for '{}' is locked by another process", domain);
                }
                let _ = fs4::FileExt::unlock(&f);
            }
        }
    }

    // 2. Dump cookies to domain jars
    jars_svc.dump_cookies_to_jars(&storage_state, &target_domains)?;

    // 3. Validate that jar_version is bumped and domain jar lock is respected
    let mut updated_count = 0;
    for (clean_domain, pre_v) in pre_versions {
        let post_jar = jars_svc
            .load_jar(&clean_domain)
            .ok_or_else(|| anyhow::anyhow!("Jar for domain '{}' was not created", clean_domain))?;

        if post_jar.jar_version <= pre_v {
            anyhow::bail!(
                "Validation failed: jar_version for domain '{}' was not bumped (was {}, now {})",
                clean_domain,
                pre_v,
                post_jar.jar_version
            );
        }

        let lock_path = jars_svc.jars_dir().join(format!(".{clean_domain}.lock"));
        if lock_path.exists() {
            let lock_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path);
            if let Ok(f) = lock_file {
                if fs4::FileExt::try_lock(&f).is_err() {
                    anyhow::bail!(
                        "Lock for domain '{}' was not properly released after dump",
                        clean_domain
                    );
                }
                let _ = fs4::FileExt::unlock(&f);
            }
        }

        let cookie_count = post_jar.cookies.len();
        println!(
            "{} Imported domain '{}' -> jar_version: {} ({} cookies)",
            "\u{2713}".green(),
            clean_domain.bold(),
            post_jar.jar_version.to_string().cyan(),
            cookie_count
        );
        updated_count += 1;
    }

    println!(
        "{} Successfully imported cookies into {} domain jar(s)",
        "\u{2713}".green().bold(),
        updated_count
    );

    Ok(())
}
