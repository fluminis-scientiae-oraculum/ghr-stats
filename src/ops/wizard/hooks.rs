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
    let new = install::rewrite_env(
        &existing,
        &started_target,
        &completed_target,
        Some(&event_log),
    );
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
            let o = privileged::run(&privileged::PrivilegedCall::Systemctl {
                verb: privileged::UnitVerb::Restart,
                unit: unit.clone(),
            });
            println!("    {}", o.describe(&format!("restart {unit}")));
        }
        None => println!(
            "    ⚠ no .service file under {} — restart the runner manually to apply",
            r.dir.display()
        ),
    }
}
