//! What ends up in the RUNNERS' OWN install dirs: job hooks, and the restarts
//! that make them take effect.
//!
//! The only half of the wizard that writes outside the config file, which is why
//! it is the only importer of `privileged`, `install` and `HookStatus` — it
//! touches files this process does not own, on behalf of a service it does not
//! run. It has also changed eleven times to the config half's four.
//!
//! Detect-first, and NEVER clobbering. An existing hook is chained rather than
//! replaced, because the runner's hook path holds at most one script and someone
//! else may already own it — overwriting would silently disable whatever was
//! there. When chaining is not safe, the wizard instructs instead of acting.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;

use crate::shared::collectors::runners;
use crate::shared::hooks::env;
use crate::shared::hooks::install::{self, HookStatus};
use crate::shared::models::RunnerInfo;
use crate::shared::paths::Scope;
use crate::shared::privileged;

use super::confirm;

// ---- runner hooks: detect-first, choose chain-or-instruct, never clobber ----

pub(super) fn hooks_step(theme: &ColorfulTheme, discovered: &[RunnerInfo]) -> Result<()> {
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
    // A root process: the scripts go in the system hooks dir every runner user
    // reads, and each runner's `.env` belongs to another user.
    if let Err(hint) = privileged::require_root("config") {
        println!(
            "  runner hooks need root — the scripts go in the system hooks dir, and \
             each runner's .env belongs to its runner user.\n  Re-run:  {hint}"
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
        match install::detect(&r.dir, std::slice::from_ref(&our_dir)) {
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

/// Already wired to us: add the per-runner event-log path if an older install
/// never set it.
fn repair_event_log(r: &RunnerInfo) {
    let Some(env) = read_env(r) else { return };
    let event_log = crate::shared::hooks::runner_event_log(&r.dir);
    match install::ensure_event_log(&env.text, &event_log) {
        None => println!("  ✓ {} already wired to ghr-stats", r.name),
        Some(new) => write_and_restart(r, &env, &new, "added missing event-log path"),
    }
}

/// Clean install: point the runner's `.env` hook vars at our scripts.
fn install_for(r: &RunnerInfo, started: &Path, completed: &Path) {
    let Some(env) = read_env(r) else { return };
    let event_log = crate::shared::hooks::runner_event_log(&r.dir);
    let new = install::rewrite_env(&env.text, started, completed, Some(&event_log));
    write_and_restart(r, &env, &new, "hooks installed");
}

/// Chain: a slot with a foreign original gets a wrapper that runs it then ours; an
/// empty slot gets our plain script. Wrappers are written before `.env`, so a
/// runner never points at a missing script.
fn chain_for(r: &RunnerInfo, our_dir: &Path, our_started: &Path, our_completed: &Path) {
    let Some(env) = read_env(r) else { return };
    let (orig_started, orig_completed) = install::current_hook_paths(&env.text);
    let [wrap_started, wrap_completed] = install::chain_wrapper_paths(our_dir, &r.dir);

    let (started_target, started_wrapper) =
        install::plan_chain_slot(orig_started.as_deref(), our_started, &wrap_started);
    let (completed_target, completed_wrapper) =
        install::plan_chain_slot(orig_completed.as_deref(), our_completed, &wrap_completed);

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
    let new = install::rewrite_env(
        &env.text,
        &started_target,
        &completed_target,
        Some(&event_log),
    );
    write_and_restart(r, &env, &new, "hooks chained");
}

fn read_env(r: &RunnerInfo) -> Option<env::EnvFile> {
    match env::read(&r.dir) {
        Ok(env) => Some(env),
        Err(e) => {
            println!("    ✗ {} — .env not rewritten: {e}", r.name);
            None
        }
    }
}

fn write_and_restart(r: &RunnerInfo, env: &env::EnvFile, new: &str, done: &str) {
    let out = env::write_env_as_root(env, new);
    if out.is_ok() {
        println!("  ✓ {} — {done}{}", r.name, env::restart_if_idle(&r.dir));
    } else {
        println!("    ✗ {}", out.describe("write .env"));
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
