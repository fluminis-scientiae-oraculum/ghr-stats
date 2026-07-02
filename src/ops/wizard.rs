//! `ghr-stats config` — consent-first interactive configuration.
//!
//! Discovers orgs from each runner's `.runner`, validates a fine-grained
//! read-only PAT per org (bounded — see `github::validate`), optionally enables
//! metrics, writes a `0600` config, and (detect-first, never clobbering) offers
//! to install/repair the runner job hooks. Nothing is read, sent, stored, or
//! changed without an explicit confirmation; tokens are masked + redacted.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password, Select};
use serde::Serialize;

use crate::shared::privileged;
use crate::shared::collectors::runners;
use crate::shared::github::validate::{self, Verdict};
use crate::shared::hooks::install::{self, HookStatus};
use crate::shared::models::RunnerInfo;
use crate::shared::paths::Scope;

pub fn run(config_override: Option<&Path>) -> Result<()> {
    let theme = ColorfulTheme::default();

    println!("ghr-stats config\n");
    println!("This will, only after you confirm each step:");
    println!("  • read each runner's .runner under the root you choose");
    println!("  • optionally validate a read-only fine-grained PAT per org");
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

    // 2) Per-org read-only PAT (bounded validation).
    println!("\n── Step 2 of 4 · Read-only GitHub PATs (optional) ──");
    let tokens = collect_tokens(&theme, &orgs, &discovered)?;

    // 3) Metrics (opt-in).
    println!("\n── Step 3 of 4 · Prometheus metrics (optional) ──");
    let metrics = prompt_metrics(&theme)?;

    // 4) Write the config (with consent), tokens redacted in the preview.
    println!("\n── Step 4 of 4 · Write config ──");
    let target = config_target(config_override);
    let write_ok = !target.exists()
        || confirm(
            &theme,
            &format!("{} exists — overwrite?", target.display()),
            false,
        )?;
    if write_ok {
        let redacted: BTreeMap<String, String> = tokens
            .keys()
            .map(|k| (k.clone(), "***".to_string()))
            .collect();
        println!("\nWill write {} (mode 0600):\n", target.display());
        println!("{}", render_config(&roots, &redacted, &metrics));
        if confirm(&theme, "Write it?", true)? {
            write_config(&target, &render_config(&roots, &tokens, &metrics))?;
            println!("✓ wrote {}", target.display());
        } else {
            println!("nothing written.");
        }
    } else {
        println!("kept existing config; nothing written.");
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

/// Collect a validated read-only PAT per org. Bounded validation: fine-grained
/// only, then read + agentId-confirm. Rejections re-prompt that org.
fn collect_tokens(
    theme: &ColorfulTheme,
    orgs: &[String],
    discovered: &[RunnerInfo],
) -> Result<BTreeMap<String, String>> {
    let mut tokens = BTreeMap::new();
    if orgs.is_empty()
        || !confirm(
            theme,
            "Add read-only GitHub PATs now? (optional; needs 'Self-hosted runners: Read')",
            false,
        )?
    {
        return Ok(tokens);
    }
    for org in orgs {
        if !confirm(theme, &format!("  Token for {org}?"), false)? {
            continue;
        }
        let local_ids: HashSet<i64> = discovered
            .iter()
            .filter(|r| &r.org == org)
            .map(|r| r.agent_id)
            .collect();
        loop {
            let token = Password::with_theme(theme)
                .with_prompt(format!("  Paste fine-grained PAT for {org}"))
                .interact()?;
            let token = token.trim().to_string();
            if token.is_empty() {
                break;
            }
            match validate::validate(&token, org, &local_ids) {
                Verdict::Valid {
                    runners,
                    matched,
                    local,
                } => {
                    println!("    ✓ valid — {runners} runners, matched {matched}/{local} local");
                    tokens.insert(org.clone(), token);
                    break;
                }
                Verdict::Rejected(why) => {
                    println!("    ✗ {why}");
                    if !confirm(theme, "    try again?", true)? {
                        break;
                    }
                }
            }
        }
    }
    Ok(tokens)
}

struct MetricsChoice {
    pull: bool,
    addr: String,
}

fn prompt_metrics(theme: &ColorfulTheme) -> Result<MetricsChoice> {
    let pull = confirm(
        theme,
        "Expose Prometheus /metrics on loopback? (served by the collector service)",
        false,
    )?;
    let addr = if pull {
        Input::with_theme(theme)
            .with_prompt("  metrics bind address (keep it on 127.0.0.1)")
            .default("127.0.0.1:9477".to_string())
            .interact_text()?
    } else {
        "127.0.0.1:9477".to_string()
    };
    Ok(MetricsChoice { pull, addr })
}

// ---- runner hooks: detect-first, choose chain-or-instruct, never clobber ----

fn hooks_step(theme: &ColorfulTheme, discovered: &[RunnerInfo]) -> Result<()> {
    if discovered.is_empty() {
        return Ok(());
    }
    println!("\nRunner job hooks record job start/completion for the Jobs view.");
    if !confirm(theme, "Install / repair runner hooks now?", false)? {
        return Ok(());
    }
    apply_hooks(theme, discovered)
}

/// Discover runners under `roots` and run the hook install/repair flow. The
/// entry point the TUI's `[h]` action uses (while suspended, on the real TTY),
/// so the per-runner detect → install/chain/instruct decisions are the same
/// ones the CLI wizard makes — one implementation, two front-ends.
pub(crate) fn install_hooks_for_tui(roots: &[PathBuf]) -> Result<()> {
    let theme = ColorfulTheme::default();
    let discovered = runners::discover(roots);
    if discovered.is_empty() {
        println!(
            "No runners found under {} — set the runner root with `ghr-stats config` first.",
            roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }
    println!(
        "Installing / repairing job hooks for {} runners (detect-first, never clobbering).\n",
        discovered.len()
    );
    apply_hooks(&theme, &discovered)
}

/// The shared hook install/repair core: gate on a root *process*, write our
/// scripts, then per runner detect → install (unset) / chain-or-instruct
/// (foreign) / no-op (ours). No initial confirm — the caller already consented
/// (the CLI wizard's prompt or the TUI's confirm popup).
fn apply_hooks(theme: &ColorfulTheme, discovered: &[RunnerInfo]) -> Result<()> {
    // Hooks are a shared *system* resource: the scripts must live where every
    // runner user can read them, and each runner's `.env` is root-owned — so
    // this needs a root *process* (System scope). `require_root` gates once here
    // (per-op sudo can't relocate our own scope); the privileged steps below run
    // via `privileged::run`. Same requirement as `systemd install --system`.
    if let Err(hint) = privileged::require_root("config") {
        println!(
            "  runner hooks need root — the scripts must be readable by the \
             runner users, and each runner's .env is root-owned.\n  Re-run:  {hint}"
        );
        return Ok(());
    }
    let our_dir = install::hooks_dir(&Scope::detect().data_dir());
    let (started, completed) = match install::install_scripts(&our_dir) {
        Ok(p) => p,
        Err(e) => {
            println!(
                "  ✗ could not write hook scripts to {} ({e}). Re-run with sudo for a system path.",
                our_dir.display()
            );
            return Ok(());
        }
    };
    println!("  hook scripts → {}", our_dir.display());

    for r in discovered {
        match install::detect(&r.dir, &our_dir) {
            HookStatus::Ours => println!("  ✓ {} already wired to ghr-stats", r.name),
            HookStatus::Unreadable => println!(
                "  ? {} — .env not readable; re-run as the runner user or root",
                r.name
            ),
            HookStatus::Unset => {
                if confirm(theme, &format!("  install hooks for {}?", r.name), true)? {
                    install_for(r, &started, &completed);
                }
            }
            HookStatus::Foreign => {
                println!(
                    "  ⚠ {} already has a job hook — ghr-stats will NOT overwrite it.",
                    r.name
                );
                let choice = Select::with_theme(theme)
                    .with_prompt(format!(
                        "    {}: how should ghr-stats add its hook?",
                        r.name
                    ))
                    .items([
                        "Chain — run your existing hook, then ghr-stats (keeps both)",
                        "Instruct — print a snippet to add to your hook yourself",
                        "Skip this runner",
                    ])
                    .default(0)
                    .interact()?;
                match choice {
                    0 => chain_for(r, &our_dir, &started, &completed),
                    1 => println!("{}", install::instruct_snippet(&our_dir)),
                    _ => println!("    skipped {}", r.name),
                }
            }
        }
    }
    Ok(())
}

/// Clean install: point the runner's `.env` hook vars at our scripts, restart.
fn install_for(r: &RunnerInfo, started: &Path, completed: &Path) {
    let env_path = r.dir.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let new = install::rewrite_env(&existing, started, completed);
    let out = crate::shared::hooks::env::write_env_as_root(&env_path, &new, &r.user);
    if out.is_ok() {
        restart_runner(r);
    } else {
        println!("    ✗ {}", out.describe("wire .env"));
    }
}

/// Chain: wrap the existing hook (keep it) + append ours, repoint `.env`, restart.
fn chain_for(r: &RunnerInfo, our_dir: &Path, our_started: &Path, our_completed: &Path) {
    let env_path = r.dir.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let (orig_started, orig_completed) = install::current_hook_paths(&existing);
    let wrap_started = our_dir.join(format!("chain-{}-started.sh", r.name));
    let wrap_completed = our_dir.join(format!("chain-{}-completed.sh", r.name));
    if let Some(o) = orig_started {
        let w = install::render_chain_wrapper(Path::new(&o), our_started);
        let _ = write_script(&wrap_started, &w);
    }
    if let Some(o) = orig_completed {
        let w = install::render_chain_wrapper(Path::new(&o), our_completed);
        let _ = write_script(&wrap_completed, &w);
    }
    let new = install::rewrite_env(&existing, &wrap_started, &wrap_completed);
    let out = crate::shared::hooks::env::write_env_as_root(&env_path, &new, &r.user);
    if out.is_ok() {
        restart_runner(r);
    } else {
        println!("    ✗ {}", out.describe("wire .env"));
    }
}

fn write_script(path: &Path, content: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(path)?;
    f.write_all(content.as_bytes())
}

fn restart_runner(r: &RunnerInfo) {
    match runners::unit_name(&r.dir) {
        Some(unit) => {
            let o = privileged::run(&["systemctl", "restart", &unit]);
            println!("    {}", o.describe(&format!("restart {unit}")));
        }
        None => println!(
            "    ⚠ no .service file under {} — restart the runner manually to apply",
            r.dir.display()
        ),
    }
}

// ---- config rendering (pure + tested) ----

#[derive(Serialize)]
struct OutConfig {
    runner_roots: Vec<String>,
    intervals: OutIntervals,
    #[serde(skip_serializing_if = "Option::is_none")]
    github: Option<OutGithub>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<OutMetrics>,
}

#[derive(Serialize)]
struct OutIntervals {
    local_secs: u64,
    api_secs: u64,
}

#[derive(Serialize)]
struct OutGithub {
    tokens: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct OutMetrics {
    pull: OutPull,
}

#[derive(Serialize)]
struct OutPull {
    enabled: bool,
    addr: String,
}

/// Render a config TOML via the serializer (proper escaping). Pure + tested.
fn render_config(
    roots: &[PathBuf],
    tokens: &BTreeMap<String, String>,
    metrics: &MetricsChoice,
) -> String {
    let out = OutConfig {
        runner_roots: roots.iter().map(|p| p.display().to_string()).collect(),
        intervals: OutIntervals {
            local_secs: 5,
            api_secs: 60,
        },
        github: (!tokens.is_empty()).then(|| OutGithub {
            tokens: tokens.clone(),
        }),
        metrics: metrics.pull.then(|| OutMetrics {
            pull: OutPull {
                enabled: true,
                addr: metrics.addr.clone(),
            },
        }),
    };
    let body = toml::to_string_pretty(&out).unwrap_or_default();
    format!("# ghr-stats config (written by `ghr-stats config`). Keep mode 0600.\n\n{body}")
}

fn write_config(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn config_target(config_override: Option<&Path>) -> PathBuf {
    crate::shared::paths::config_write_target(config_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_metrics() -> MetricsChoice {
        MetricsChoice {
            pull: false,
            addr: "127.0.0.1:9477".to_string(),
        }
    }

    #[test]
    fn render_round_trips_into_config() {
        let mut tokens = BTreeMap::new();
        tokens.insert("example-org".to_string(), "github_pat_xyz".to_string());
        let toml = render_config(&[PathBuf::from("/srv/runners")], &tokens, &no_metrics());

        assert!(toml.contains("[github.tokens]"));
        assert!(toml.contains("example-org"));
        let cfg: crate::shared::config::Config =
            toml::from_str(&toml).expect("generated config parses");
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/runners")]);
        assert_eq!(
            cfg.github_token_for("example-org").as_deref(),
            Some("github_pat_xyz")
        );
    }

    #[test]
    fn render_with_metrics_enables_pull() {
        let m = MetricsChoice {
            pull: true,
            addr: "127.0.0.1:9999".to_string(),
        };
        let toml = render_config(&[PathBuf::from("/x")], &BTreeMap::new(), &m);
        assert!(toml.contains("[metrics.pull]"));
        let cfg: crate::shared::config::Config = toml::from_str(&toml).expect("parses");
        assert!(cfg.metrics.pull.enabled);
        assert_eq!(cfg.metrics.pull.addr, "127.0.0.1:9999");
    }

    #[test]
    fn render_without_tokens_or_metrics_omits_sections() {
        let toml = render_config(&[PathBuf::from("/x")], &BTreeMap::new(), &no_metrics());
        assert!(!toml.contains("[github"));
        assert!(!toml.contains("[metrics"));
        let _cfg: crate::shared::config::Config = toml::from_str(&toml).expect("parses");
    }
}
