//! `ghr-stats config` — consent-first interactive configuration.
//!
//! Discovers orgs from each runner's `.runner`, validates a fine-grained
//! read-only PAT per org (bounded — see `github::validate`), optionally enables
//! metrics, writes a `0600` config, and (detect-first, never clobbering) offers
//! to install/repair the runner job hooks. Nothing is read, sent, stored, or
//! changed without an explicit confirmation; tokens are masked + redacted.
//!
//! Cut by WHAT THE WIZARD WRITES TO:
//!
//! - **this file** — the run itself: the consent preamble, the ordered steps, and
//!   the prompt primitives every step shares.
//! - [`config`] — everything that ends up in the one `0600` config file: the
//!   per-org token plan and the metrics choice, applied as faithful in-place
//!   `persist` edits that preserve every OTHER setting.
//! - [`hooks`] — everything that ends up in the RUNNERS' OWN install dirs: the
//!   detect-first hook install, the chain-or-instruct decision, the event-log
//!   repair, and the service restarts.
//!
//! The hook half is the only part that leaves the config file, and that shows in
//! both seam tests. It is the only importer of `privileged`, `install::` and
//! `HookStatus` — because it is the only part that touches files this process
//! does not own — and it has changed ELEVEN times to the config half's four.
//! Those are two different rates of change on two different targets.

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

    // 1) Discover runners — auto-detect the root(s) from systemd; ask only when
    // that finds nothing, so most users just press Enter.
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

    // The config file we read the current PAT state from and write back to.
    let target = config_target(config_override);

    // 2) Per-org read-only PAT: add / replace / remove (bounded validation). We
    // read which orgs already have a PAT so each one offers the right action.
    println!("\n── Step 2 of 4 · Read-only GitHub PATs (optional) ──");
    println!(
        "  Fine-grained PAT per org. Required: Organization → Self-hosted runners → Read-only.\n  \
         Optional: Repository → Actions → Read-only (fills each job's success/failure — needs\n  \
         repo access set to All/selected repos, NOT \"Public repositories\")."
    );
    let existing = existing_token_orgs(&target);
    let plan = manage_tokens(&theme, &discovered, &existing)?;

    // 3) Metrics (opt-in).
    println!("\n── Step 3 of 4 · Prometheus metrics (optional) ──");
    let metrics = prompt_metrics(&theme)?;

    // 4) Update the config as FAITHFUL in-place edits (never a full rewrite): we
    // change only what you set this run and preserve every other setting — an
    // existing PAT you don't touch, the push metrics config, custom intervals,
    // the org list. Re-running `config` is therefore safe and non-destructive.
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

    // 5) Runner hooks (opt-in, detect-first).
    hooks_step(&theme, &discovered)?;

    Ok(())
}

fn confirm(theme: &ColorfulTheme, prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::with_theme(theme)
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

/// Pick the runner install root(s). Auto-detected from systemd (the common case
/// — just press Enter), or entered manually when detection finds nothing.
/// Always returns at least one root (the manual fallback yields one).
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

/// Expand a leading `~/` using `$HOME` (dialoguer returns the raw string).
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
