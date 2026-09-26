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

use config::{MetricsChoice, apply_config, existing_token_keys, manage_tokens, prompt_metrics};
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
    let target = config_target(config_override);
    if !writable(&target) {
        anyhow::bail!(
            "{} is not writable by this user — re-run `{}`",
            target.display(),
            crate::shared::privileged::sudo_hint("config")
        );
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

    println!("\n── Step 2 of 4 · Read-only GitHub PATs (optional) ──");
    println!(
        "  Fine-grained PAT per org or account. Organization runners need Organization →\n  \
         Self-hosted runners → Read; repository runners need Repository → Administration →\n  \
         Read. Optional: Repository → Actions → Read fills each job's success/failure (repo\n  \
         access must include those repositories)."
    );
    let existing = existing_token_keys(&target);
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
        for key in plan.set.keys() {
            println!("  github.tokens.\"{key}\" = *** (set/replaced)");
        }
        for key in &plan.remove {
            println!("  github.tokens.\"{key}\" = REMOVED (org forgotten)");
        }
    }
    match metrics {
        MetricsChoice::Pull(addr) => println!("  metrics.pull = enabled @ {addr}"),
        MetricsChoice::Unchanged => println!("  metrics: unchanged"),
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

/// `~` is the invoking user's home, also under `sudo`.
fn expand_tilde(s: &str) -> String {
    use uzers::os::unix::UserExt;
    let Some(rest) = s.strip_prefix("~/") else {
        return s.to_string();
    };
    let home = std::env::var_os("SUDO_USER")
        .filter(|_| crate::shared::privileged::is_root())
        .and_then(|u| uzers::get_user_by_name(&u))
        .map(|u| u.home_dir().to_path_buf())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    match home {
        Some(home) => format!("{}/{rest}", home.display()),
        None => s.to_string(),
    }
}

/// Whether this process can create or replace the config at `target`.
fn writable(target: &Path) -> bool {
    use nix::unistd::{AccessFlags, access};
    let probe = if target.exists() {
        target.to_path_buf()
    } else {
        target
            .ancestors()
            .skip(1)
            .find(|p| p.exists())
            .unwrap_or(Path::new("/"))
            .to_path_buf()
    };
    access(&probe, AccessFlags::W_OK).is_ok()
}

fn config_target(config_override: Option<&Path>) -> PathBuf {
    crate::shared::paths::config_write_target(config_override)
}
