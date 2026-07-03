//! `ghr-stats config` — consent-first interactive configuration.
//!
//! Discovers orgs from each runner's `.runner`, validates a fine-grained
//! read-only PAT per org (bounded — see `github::validate`), optionally enables
//! metrics, writes a `0600` config, and (detect-first, never clobbering) offers
//! to install/repair the runner job hooks. Nothing is read, sent, stored, or
//! changed without an explicit confirmation; tokens are masked + redacted.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password, Select};

use crate::shared::privileged;
use crate::shared::collectors::runners;
use crate::shared::config::persist;
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

    // The config file we read the current PAT state from and write back to.
    let target = config_target(config_override);

    // 2) Per-org read-only PAT: add / replace / remove (bounded validation). We
    // read which orgs already have a PAT so each one offers the right action.
    println!("\n── Step 2 of 4 · Read-only GitHub PATs (optional) ──");
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

/// The per-org PAT changes the token step decided: orgs to set/replace (with the
/// validated token) and orgs to remove. Applied via faithful `persist` edits.
#[derive(Default)]
struct TokenPlan {
    set: BTreeMap<String, String>,
    remove: BTreeSet<String>,
}

impl TokenPlan {
    fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// The org logins that already have a PAT in `target` — presence only, read from
/// the file text (so it survives schema drift). Empty when the file is absent or
/// unreadable (a non-root run can't read the root-owned `/etc` config, so it
/// degrades to add-only rather than failing).
fn existing_token_orgs(target: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(target)
        .ok()
        .map(|t| crate::shared::config::token_orgs(&t).into_iter().collect())
        .unwrap_or_default()
}

/// Per-org PAT management: for an org that already has a PAT, offer keep /
/// replace / remove; for one without, offer to add. Candidates are the union of
/// discovered orgs and orgs that already hold a PAT (so a stale one — whose
/// runners are gone — can still be removed). Bounded validation on set/replace.
fn manage_tokens(
    theme: &ColorfulTheme,
    discovered: &[RunnerInfo],
    existing: &BTreeSet<String>,
) -> Result<TokenPlan> {
    let mut plan = TokenPlan::default();
    let mut candidates: BTreeSet<String> = discovered.iter().map(|r| r.org.clone()).collect();
    candidates.extend(existing.iter().cloned());
    if candidates.is_empty()
        || !confirm(
            theme,
            "Manage read-only GitHub PATs now? (add / replace / remove; needs 'Self-hosted runners: Read')",
            false,
        )?
    {
        return Ok(plan);
    }
    for org in &candidates {
        let local_ids: HashSet<i64> = discovered
            .iter()
            .filter(|r| &r.org == org)
            .map(|r| r.agent_id)
            .collect();
        if existing.contains(org) {
            let choice = Select::with_theme(theme)
                .with_prompt(format!("  {org} already has a PAT — action?"))
                .items(["Keep it", "Replace the PAT", "Remove it (forget this org)"])
                .default(0)
                .interact()?;
            match choice {
                1 => {
                    if let Some(t) = prompt_validated_pat(theme, org, &local_ids)? {
                        plan.set.insert(org.clone(), t);
                    }
                }
                2 => {
                    plan.remove.insert(org.clone());
                    println!("    • will remove {org}'s PAT");
                }
                _ => {}
            }
        } else if confirm(theme, &format!("  Add a token for {org}?"), false)?
            && let Some(t) = prompt_validated_pat(theme, org, &local_ids)?
        {
            plan.set.insert(org.clone(), t);
        }
    }
    Ok(plan)
}

/// Prompt for a fine-grained PAT and validate it (fine-grained only, read +
/// agentId-confirm). `Some(token)` once valid; `None` if left blank or the user
/// gives up after a rejection. Shared by the add and replace paths.
fn prompt_validated_pat(
    theme: &ColorfulTheme,
    org: &str,
    local_ids: &HashSet<i64>,
) -> Result<Option<String>> {
    loop {
        let token = Password::with_theme(theme)
            .with_prompt(format!("  Paste fine-grained PAT for {org}"))
            .interact()?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        match validate::validate(&token, org, local_ids) {
            Verdict::Valid {
                runners,
                matched,
                local,
            } => {
                println!("    ✓ valid — {runners} runners, matched {matched}/{local} local");
                return Ok(Some(token));
            }
            Verdict::Rejected(why) => {
                println!("    ✗ {why}");
                if !confirm(theme, "    try again?", true)? {
                    return Ok(None);
                }
            }
        }
    }
}

/// Apply the wizard's decisions as faithful in-place `persist` edits: set the
/// roots, set/replace the collected PATs, remove the ones marked for removal, and
/// enable metrics if chosen. Every OTHER setting in the file is preserved. Pure
/// of prompts (all consent happened already), so it is unit-testable end-to-end.
fn apply_config(
    target: &Path,
    roots: &[PathBuf],
    plan: &TokenPlan,
    metrics: &MetricsChoice,
) -> Result<()> {
    persist::set_runner_roots(target, roots)?;
    for (org, token) in &plan.set {
        persist::set_org_token(target, org, token)?;
    }
    for org in &plan.remove {
        persist::remove_org_token(target, org)?;
    }
    // Only touch metrics when enabling — declining leaves any existing pull/push
    // config alone rather than clobbering it.
    if metrics.pull {
        persist::set_metrics_pull(target, true, &metrics.addr)?;
    }
    Ok(())
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
            HookStatus::Ours => repair_event_log(r),
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

/// Already wired to us: ensure the runner's `.env` also carries its per-runner
/// event-log path, then restart if we had to add it. This makes `config` a
/// self-healing upgrade path — a runner wired by a version that predates
/// `GHR_STATS_EVENT_LOG` (detected `Ours`, so install/chain are skipped) would
/// otherwise never emit events.
fn repair_event_log(r: &RunnerInfo) {
    let env_path = r.dir.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let event_log = crate::shared::hooks::runner_event_log(&r.dir);
    match install::ensure_event_log(&existing, &event_log) {
        None => println!("  ✓ {} already wired to ghr-stats", r.name),
        Some(new) => {
            let out = crate::shared::hooks::env::write_env_as_root(&env_path, &new, &r.user);
            if out.is_ok() {
                println!("  ✓ {} — added missing event-log path", r.name);
                restart_runner(r);
            } else {
                println!("    ✗ {}", out.describe("repair .env"));
            }
        }
    }
}

/// Clean install: point the runner's `.env` hook vars at our scripts, restart.
fn install_for(r: &RunnerInfo, started: &Path, completed: &Path) {
    let env_path = r.dir.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let event_log = crate::shared::hooks::runner_event_log(&r.dir);
    let new = install::rewrite_env(&existing, started, completed, Some(&event_log));
    let out = crate::shared::hooks::env::write_env_as_root(&env_path, &new, &r.user);
    if out.is_ok() {
        restart_runner(r);
    } else {
        println!("    ✗ {}", out.describe("wire .env"));
    }
}

/// Chain: wrap the existing hook (keep it) + append ours, repoint `.env`, restart.
/// Per-slot: a slot with a foreign original gets a wrapper; a slot with no
/// original (a `Foreign` runner with only ONE hook var set) is wired to our plain
/// script directly — never to a wrapper we didn't write. Any wrapper write must
/// succeed before we touch `.env`, so we can't point a runner at a missing script.
fn chain_for(r: &RunnerInfo, our_dir: &Path, our_started: &Path, our_completed: &Path) {
    let env_path = r.dir.join(".env");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let (orig_started, orig_completed) = install::current_hook_paths(&existing);
    let wrap_started = our_dir.join(format!("chain-{}-started.sh", r.name));
    let wrap_completed = our_dir.join(format!("chain-{}-completed.sh", r.name));

    let (started_target, started_wrapper) =
        install::plan_chain_slot(orig_started.as_deref(), our_started, &wrap_started);
    let (completed_target, completed_wrapper) =
        install::plan_chain_slot(orig_completed.as_deref(), our_completed, &wrap_completed);

    // Write any wrappers FIRST; abort without touching `.env` if a write fails —
    // never leave a runner pointed at a wrapper that isn't on disk.
    for (path, content) in [started_wrapper, completed_wrapper].into_iter().flatten() {
        if let Err(e) = write_script(&path, &content) {
            println!(
                "    ✗ {} — could not write {} ({e}); .env left unchanged",
                r.name,
                path.display()
            );
            return;
        }
    }

    let event_log = crate::shared::hooks::runner_event_log(&r.dir);
    let new = install::rewrite_env(&existing, &started_target, &completed_target, Some(&event_log));
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

fn config_target(config_override: Option<&Path>) -> PathBuf {
    crate::shared::paths::config_write_target(config_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wizard's apply step, end-to-end against a real config file: set a new
    /// PAT, replace an existing one, remove another, and set the roots — while
    /// every untouched setting (here the push config) survives. This is the
    /// CLI-side of add/replace/remove, proven without the interactive prompts.
    #[test]
    fn apply_config_sets_replaces_removes_and_preserves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/old\"]\n\
             [github.tokens]\nacme = \"github_pat_OLD\"\nwidgets = \"github_pat_W\"\n\
             [metrics.push]\nenabled = true\n",
        )
        .unwrap();

        let mut plan = TokenPlan::default();
        plan.set.insert("acme".into(), "github_pat_NEW".into()); // replace
        plan.set.insert("beta".into(), "github_pat_B".into()); // add
        plan.remove.insert("widgets".into()); // remove
        let metrics = MetricsChoice {
            pull: false,
            addr: "127.0.0.1:9477".into(),
        };

        apply_config(&path, &[PathBuf::from("/srv/r")], &plan, &metrics).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: crate::shared::config::Config = toml::from_str(&text).unwrap();
        // Per-org tokens take precedence over env/fallback, so these are deterministic.
        assert_eq!(cfg.github_token_for("acme").as_deref(), Some("github_pat_NEW"));
        assert_eq!(cfg.github_token_for("beta").as_deref(), Some("github_pat_B"));
        // widgets removed (presence check is env-independent).
        assert!(!cfg.github.tokens.contains_key("widgets"));
        assert!(!text.contains("github_pat_W"));
        // Untouched settings + the new roots.
        assert!(cfg.metrics.push.enabled);
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/r")]);
    }

    #[test]
    fn existing_token_orgs_reads_configured_orgs_empty_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(existing_token_orgs(&path).is_empty()); // no file yet
        std::fs::write(
            &path,
            "[github.tokens]\nacme = \"github_pat_A\"\nwidgets = \"github_pat_W\"\n",
        )
        .unwrap();
        let got = existing_token_orgs(&path);
        assert_eq!(got.len(), 2);
        assert!(got.contains("acme") && got.contains("widgets"));
    }
}
