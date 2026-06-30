mod cli;
mod collectors;
mod config;
mod daemon;
mod error;
mod model;
mod setup;
mod store;
mod tui;
mod util;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command, DbAction};

fn main() -> Result<()> {
    init_tracing();
    let args = Cli::parse();
    let cfg = config::Config::load(args.config.as_deref()).context("loading config")?;

    match args.command {
        Command::Db { action } => run_db(action, &cfg),
        Command::Collect => daemon::run(&cfg),
        Command::Tui => tui::run(&cfg),
        Command::Setup => setup::run(args.config.as_deref()),
        Command::Github => run_github(&cfg),
    }
}

/// Validate each org's PAT and list its runners, cross-checked against the
/// locally-discovered runners by `agentId`. Never prints the token.
fn run_github(cfg: &config::Config) -> Result<()> {
    let local = collectors::runners::discover(&cfg.runner_roots);
    let orgs = if cfg.orgs.is_empty() {
        let mut o: Vec<String> = local.iter().map(|r| r.org.clone()).collect();
        o.sort();
        o.dedup();
        o
    } else {
        cfg.orgs.clone()
    };

    if orgs.is_empty() {
        println!("No orgs to check (set [github] orgs, or run where runners are discoverable).");
        return Ok(());
    }

    for org in &orgs {
        let Some(token) = cfg.github_token_for(org) else {
            println!(
                "•  {org}: no token (set [github.tokens].\"{org}\" or GHR_STATS_GITHUB_TOKEN)"
            );
            continue;
        };
        match collectors::github::list_org_runners(&token, org) {
            Ok(api) => {
                let online = api.iter().filter(|r| r.status == "online").count();
                let busy = api.iter().filter(|r| r.busy).count();
                let local_ids: std::collections::HashSet<i64> = local
                    .iter()
                    .filter(|r| &r.org == org)
                    .map(|r| r.agent_id)
                    .collect();
                let matched = api.iter().filter(|r| local_ids.contains(&r.id)).count();
                println!(
                    "✓  {org}: authenticated — {} runners ({online} online, {busy} busy); \
                     matched {matched}/{} local by agentId",
                    api.len(),
                    local_ids.len()
                );
            }
            Err(e) => println!("✗  {e}"),
        }
    }
    Ok(())
}

fn run_db(action: DbAction, cfg: &config::Config) -> Result<()> {
    match action {
        DbAction::Init => {
            store::Store::open(&cfg.db_path)
                .with_context(|| format!("opening db at {}", cfg.db_path.display()))?;
            println!("initialized {}", cfg.db_path.display());
            Ok(())
        }
        DbAction::Prune { days } => {
            let mut store = store::Store::open(&cfg.db_path)
                .with_context(|| format!("opening db at {}", cfg.db_path.display()))?;
            let cutoff = util::now_epoch() - (days as i64) * 86_400;
            let removed = store::writer::prune(store.conn_mut(), cutoff)?;
            println!("pruned {removed} sample rows older than {days}d");
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
