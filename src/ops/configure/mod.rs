//! `ghr-stats config`: nothing is read, sent, stored or changed without explicit
//! confirmation; tokens are masked and redacted. [`config`] edits the config file,
//! [`hooks`] the runners' own install dirs.

use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input};

use crate::shared::collectors::runners;

mod config;
mod hooks;

use config::{apply_config, existing_token_orgs, manage_tokens, prompt_metrics};
use hooks::hooks_step;

pub(crate) use hooks::install_hooks_for_tui;

pub fn run(config_override: Option<&Path>) -> Result<()> {
    let theme = ColorfulTheme::default();

    println!("ghr-stats config\n");
    println!("This will, only after you confirm each step:");
    println!("  • read each runner's .runner under the root you choose");
    println!(
        "  • optionally validate a read-only fine-grained PAT per org \
         (Self-hosted runners: Read; + Actions: Read for job results)"
    );
    println!("  • optionally enable Prometheus metrics");
    println!(
        "  • optionally install/repair the runner job hooks (never clobbering an existing one)"
    );
    println!("  • write a config file (mode 0600)\n");
    if !confirm(&theme, "Proceed?", true)? {
        println!("aborted.");
        return Ok(());
    }

    println!("── Step 1 of 4 · Discover runners ──");
    let roots = choose_roots(&theme)?;
    let discovered = runners::discover(&roots);
    let mut orgs: Vec<String> = discovered.iter().map(|r| r.org.clone()).collect();
    orgs.sort();
    orgs.dedup();
    if discovered.is_empty() {
        let where_ = roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        println!("⚠ no runners found under {where_} (no .runner files).");
    } else {
        println!(
            "found {} runners across {} orgs: {}",
            discovered.len(),
            orgs.len(),
            orgs.join(", ")
        );
    }

    let target = config_target(config_override);

    println!("\n── Step 2 of 4 · Read-only GitHub PATs (optional) ──");
    println!(
        "  Fine-grained PAT per org. Required: Organization → Self-hosted runners → Read-only.\n  \
         Optional: Repository → Actions → Read-only (fills each job's success/failure — needs\n  \
         repo access set to All/selected repos, NOT \"Public repositories\")."
    );
    let existing = existing_token_orgs(&target);
    let plan = manage_tokens(&theme, &discovered, &existing)?;

    println!("\n── Step 3 of 4 · Prometheus metrics (optional) ──");
    let metrics = prompt_metrics(&theme)?;

    println!("\n── Step 4 of 4 · Update config ──");
    println!(
        "\nWill update {} (mode 0600), preserving every other setting \
         (existing PATs, push, intervals):",
        target.display()
    );
    println!(
        "  runner_roots = [{}]",
        roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if plan.is_empty() {
        println!("  github tokens: unchanged (existing PATs kept)");
    } else {
        for org in plan.set.keys() {
            println!("  github.tokens.{org} = *** (set/replaced)");
        }
        for org in &plan.remove {
            println!("  github.tokens.{org} = REMOVED (org forgotten)");
        }
    }
    if metrics.pull {
        println!("  metrics.pull = enabled @ {}", metrics.addr);
    } else {
        println!("  metrics: unchanged");
    }
    if confirm(&theme, "Apply these changes?", true)? {
        apply_config(&target, &roots, &plan, &metrics)?;
        println!(
            "✓ updated {} (existing PATs and other settings preserved)",
            target.display()
        );
    } else {
        println!("no changes written.");
    }

    hooks_step(&theme, &discovered)?;

    Ok(())
}

fn confirm(theme: &ColorfulTheme, prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::with_theme(theme)
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

/// Never empty: the manual fallback yields one root.
fn choose_roots(theme: &ColorfulTheme) -> Result<Vec<PathBuf>> {
    let found = runners::discover_roots();
    if !found.is_empty() {
        println!("Auto-detected runner install dir(s) under:");
        for r in &found {
            println!("  • {}", r.display());
        }
        if confirm(theme, "Use these?", true)? {
            return Ok(found);
        }
    } else {
        println!(
            "Couldn't auto-detect from systemd (no actions.runner.* services, or systemctl is \
             unavailable) — enter the path manually."
        );
    }
    let root: String = Input::with_theme(theme)
        .with_prompt(
            "Runner install root — the directory that holds your runner install dirs (each has a \
             .runner file), e.g. /opt/actions-runner or ~/actions-runner",
        )
        .interact_text()?;
    Ok(vec![PathBuf::from(expand_tilde(root.trim()))])
}

fn expand_tilde(s: &str) -> String {
    match s.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => format!("{}/{}", home.to_string_lossy(), rest),
            None => s.to_string(),
        },
        None => s.to_string(),
    }
}

fn config_target(config_override: Option<&Path>) -> PathBuf {
    crate::shared::paths::config_write_target(config_override)
}
