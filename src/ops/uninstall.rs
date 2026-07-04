//! `ghr-stats uninstall` — the honest inverse of install, safe by default.
//!
//! Two phases: DETECT + PLAN (read-only), then CONFIRM + APPLY. A bare
//! `uninstall` runs only the first phase over every domain — a redacted dry-run
//! that removes nothing. Domain flags (or `--all`) opt into removal; you confirm
//! first unless `--yes`.
//!
//! Nothing sensitive is ever printed: config tokens are shown as a COUNT, never a
//! value, and runner `.env` contents are never echoed. Hooks are reverted
//! detect-first (see [`crate::shared::hooks::uninstall`]) so a foreign hook is never
//! stranded. The receipt is stdout-only — uninstall leaves nothing behind.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::cli::{UninstallArgs, UninstallDomain};
use crate::ops::systemd;
use crate::shared::collectors::{procscan, runners};
use crate::shared::hooks::install;
use crate::shared::hooks::uninstall::{self as hook_revert, RevertAction, RunnerHookPlan};
use crate::shared::paths::{self, Scope};
use crate::shared::privileged;

pub fn run(args: &UninstallArgs, config_override: Option<&Path>) -> Result<()> {
    let scope = systemd::resolve_scope(args.system, args.user);
    let preview = args.domains.is_empty(); // a bare `uninstall` previews everything
    if args.yes && preview {
        bail!(
            "nothing selected — name a domain (hooks/service/config/data/binary/all). \
             (A bare `uninstall` is a dry-run and needs no --yes.)"
        );
    }
    let domains = if preview {
        Domains::all()
    } else {
        Domains::from_args(args)
    };
    let execute = !preview;

    // Refuse a partial system-scope teardown up front: /etc, /var/lib,
    // /usr/local/bin and the system unit all need a root process. Better to stop
    // clean than to remove some artifacts and fail on the rest.
    if execute && scope == Scope::System && !privileged::is_root() {
        bail!(
            "system-scope uninstall needs root — re-run `{}`",
            privileged::sudo_hint("uninstall")
        );
    }

    let plan = Plan::detect(scope, domains, config_override);
    plan.render(execute);

    if !execute {
        println!(
            "\nDry run — nothing was removed. Name a domain to remove \
             (e.g. `uninstall hooks` or `uninstall all`); add --yes to skip the confirm.\n\
             See `ghr-stats uninstall --help`."
        );
        return Ok(());
    }
    if !plan.has_actions() {
        println!("\nNothing to remove.");
        return Ok(());
    }
    if !args.yes && !confirm() {
        println!("aborted — nothing removed.");
        return Ok(());
    }
    println!();
    plan.apply();
    Ok(())
}

/// The five orthogonal removal domains.
#[derive(Clone, Copy)]
struct Domains {
    hooks: bool,
    service: bool,
    config: bool,
    data: bool,
    binary: bool,
}

impl Domains {
    fn from_args(a: &UninstallArgs) -> Self {
        if a.domains.contains(&UninstallDomain::All) {
            return Self::all();
        }
        let has = |d: UninstallDomain| a.domains.contains(&d);
        Self {
            hooks: has(UninstallDomain::Hooks),
            service: has(UninstallDomain::Service),
            config: has(UninstallDomain::Config),
            data: has(UninstallDomain::Data),
            binary: has(UninstallDomain::Binary),
        }
    }
    fn all() -> Self {
        Self {
            hooks: true,
            service: true,
            config: true,
            data: true,
            binary: true,
        }
    }
}

/// What removing the binary means on this host. Pure result of [`binary_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum BinaryAction {
    /// A `systemd install` copy at this path — safe to remove (even if running).
    Remove(PathBuf),
    /// Running from a `cargo install` build — we don't own it; print the command.
    InstructCargo(PathBuf),
    /// No installed copy found.
    NotInstalled(PathBuf),
}

/// Decide the binary action from the (would-be) installed path, whether it
/// exists, and the running exe. Pure + tested. A `cargo install` build is never
/// deleted by us — Cargo owns `~/.cargo/bin`; we print `cargo uninstall`.
fn binary_action(
    installed: &Path,
    installed_exists: bool,
    current_exe: Option<&Path>,
) -> BinaryAction {
    if installed_exists {
        return BinaryAction::Remove(installed.to_path_buf());
    }
    if let Some(exe) = current_exe
        && is_cargo_bin(exe)
    {
        return BinaryAction::InstructCargo(exe.to_path_buf());
    }
    BinaryAction::NotInstalled(installed.to_path_buf())
}

/// Whether `exe` lives in a Cargo bin dir (`…/.cargo/bin/<exe>`).
fn is_cargo_bin(exe: &Path) -> bool {
    exe.parent().is_some_and(|p| p.ends_with(".cargo/bin"))
}

/// One config file slated for removal + how many tokens it holds (redacted).
struct ConfigItem {
    path: PathBuf,
    token_count: Option<usize>,
}

/// The detected, previewable teardown — built read-only, then rendered + applied.
struct Plan {
    scope: Scope,
    domains: Domains,
    our_dir: PathBuf,
    runners: Vec<RunnerHookPlan>,
    service_unit: Option<PathBuf>, // Some(path) iff the unit file is present
    binary: Option<BinaryAction>,
    config: Vec<ConfigItem>,
    data: Vec<PathBuf>,
    cross_scope: Vec<String>,
}

impl Plan {
    fn detect(scope: Scope, domains: Domains, config_override: Option<&Path>) -> Self {
        let our_dir = install::hooks_dir(&scope.data_dir());

        let runners = if domains.hooks {
            discover_runners(config_override)
                .iter()
                .map(|r| hook_revert::plan_runner(r, &our_dir))
                .collect()
        } else {
            Vec::new()
        };

        let service_unit = if domains.service {
            let p = scope.systemd_unit_path();
            p.exists().then_some(p)
        } else {
            None
        };

        let binary = domains.binary.then(|| {
            let installed = scope.bin_path();
            let exists = installed.exists();
            binary_action(&installed, exists, std::env::current_exe().ok().as_deref())
        });

        let config = if domains.config {
            config_candidates(scope, config_override)
                .into_iter()
                .filter(|p| p.exists())
                .map(|path| {
                    let token_count = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|t| crate::shared::config::count_tokens(&t));
                    ConfigItem { path, token_count }
                })
                .collect()
        } else {
            Vec::new()
        };

        let data = if domains.data {
            data_files(scope)
                .into_iter()
                .filter(|p| p.exists())
                .collect()
        } else {
            Vec::new()
        };

        let cross_scope = cross_scope_probe(scope);

        Self {
            scope,
            domains,
            our_dir,
            runners,
            service_unit,
            binary,
            config,
            data,
            cross_scope,
        }
    }

    /// Anything actually removable (used to short-circuit an all-clean execute).
    fn has_actions(&self) -> bool {
        self.runners.iter().any(|r| {
            matches!(
                r.action,
                RevertAction::Strip { .. } | RevertAction::Restore { .. }
            )
        }) || self.service_unit.is_some()
            || matches!(self.binary, Some(BinaryAction::Remove(_)))
            || !self.config.is_empty()
            || !self.data.is_empty()
    }

    fn render(&self, execute: bool) {
        let tag = if execute {
            ""
        } else {
            "  ·  DRY RUN (nothing will be removed)"
        };
        println!("ghr-stats uninstall — {} scope{tag}", self.scope.label());

        if self.domains.hooks {
            println!("\nHooks  ({}):", self.our_dir.display());
            if self.runners.is_empty() {
                println!("  (no runners discovered)");
            }
            for rp in &self.runners {
                println!("{}", plan_line(rp));
            }
            if !self.runners.is_empty() && !privileged::is_root() {
                println!("  ⚠ reverting hooks edits root-owned .env files — re-run with sudo");
            }
        }

        if self.domains.service {
            println!("\nService:");
            match &self.service_unit {
                Some(p) => println!("  remove {}", p.display()),
                None => println!("  (not installed)"),
            }
        }

        if self.domains.binary {
            println!("\nBinary:");
            match &self.binary {
                Some(BinaryAction::Remove(p)) => println!("  remove {}", p.display()),
                Some(BinaryAction::InstructCargo(p)) => println!(
                    "  {} is a `cargo install` build — run `cargo uninstall ghr-stats`",
                    p.display()
                ),
                Some(BinaryAction::NotInstalled(p)) => {
                    println!("  (no installed copy at {})", p.display())
                }
                None => {}
            }
        }

        if self.domains.config {
            println!("\nConfig:");
            if self.config.is_empty() {
                println!("  (no config file)");
            }
            let mut any_tokens = false;
            for c in &self.config {
                let tok = match c.token_count {
                    Some(0) => "no tokens".to_string(),
                    Some(n) => {
                        any_tokens = true;
                        format!("{n} redacted token(s)")
                    }
                    None => "unreadable".to_string(),
                };
                println!(
                    "  remove {}  ({tok}; unlinked, not shredded)",
                    c.path.display()
                );
            }
            if any_tokens {
                println!(
                    "  ↳ tokens are only unlinked — revoke them on GitHub \
                     (Settings → Developer settings) to be sure"
                );
            }
        }

        if self.domains.data {
            println!("\nData:");
            if self.data.is_empty() {
                println!("  (no data files)");
            }
            for p in &self.data {
                println!("  remove {}", p.display());
            }
        }

        if !self.cross_scope.is_empty() {
            println!("\nOther scope (not touched by this run):");
            for l in &self.cross_scope {
                println!("  {l}");
            }
        }
    }

    fn apply(&self) {
        // Hooks first: revert runners, then GC the shared scripts if orphaned.
        if self.domains.hooks {
            println!("Hooks:");
            if !privileged::is_root() {
                println!(
                    "  ⚠ skipped — reverting hooks needs root; re-run `{}`",
                    privileged::sudo_hint("uninstall --hooks")
                );
            } else {
                let procs = procscan::scan();
                for rp in &self.runners {
                    let idle = hook_revert::is_idle(rp.uid, &procs);
                    println!("{}", hook_revert::apply_runner(rp, idle));
                }
                gc_shared_scripts(&self.our_dir, &self.runners);
            }
        }

        if let Some(unit) = &self.service_unit {
            println!("Service:");
            match systemd::uninstall(self.scope) {
                Ok(()) => {}
                Err(e) => println!("  ✗ {} — {e}", unit.display()),
            }
        }

        if let Some(BinaryAction::Remove(p)) = &self.binary {
            println!("Binary:");
            match std::fs::remove_file(p) {
                Ok(()) => println!("  ✓ removed {}", p.display()),
                Err(e) => println!("  ✗ {} — {e}", p.display()),
            }
        }

        if !self.config.is_empty() {
            println!("Config:");
            for c in &self.config {
                remove_reporting(&c.path);
                let _ = c.path.parent().map(std::fs::remove_dir); // only if empty
            }
        }

        if !self.data.is_empty() {
            println!("Data:");
            for p in &self.data {
                remove_reporting(p);
            }
            let _ = std::fs::remove_dir(self.scope.data_dir()); // only if empty
        }

        println!("\nDone.");
    }
}

/// Remove the shared `job-*.sh` scripts + the hooks dir — but only once no
/// runner's live `.env` still points into it (a foreign/unreverted runner might).
fn gc_shared_scripts(our_dir: &Path, plans: &[RunnerHookPlan]) {
    let still_referenced = plans
        .iter()
        .any(|rp| env_points_into(&rp.env_path, our_dir));
    if still_referenced {
        println!(
            "  · kept {} — still referenced by a runner not managed by ghr-stats",
            our_dir.display()
        );
        return;
    }
    let _ = std::fs::remove_file(our_dir.join("job-started.sh"));
    let _ = std::fs::remove_file(our_dir.join("job-completed.sh"));
    if std::fs::remove_dir(our_dir).is_ok() {
        println!("  ✓ removed shared hook scripts {}", our_dir.display());
    }
}

/// Whether a runner's current `.env` still points a hook var inside `our_dir`.
fn env_points_into(env_path: &Path, our_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(env_path) else {
        return false;
    };
    let (s, c) = install::current_hook_paths(&text);
    [s, c]
        .into_iter()
        .flatten()
        .any(|v| Path::new(&v).starts_with(our_dir))
}

fn remove_reporting(p: &Path) {
    match std::fs::remove_file(p) {
        Ok(()) => println!("  ✓ removed {}", p.display()),
        Err(e) => println!("  ✗ {} — {e}", p.display()),
    }
}

/// Runner install roots for hook reversal — a loadable config's roots if present,
/// else auto-detected from systemd (same as the wizard's Step 1). Config-free so
/// hooks can be reverted even after the config is gone.
fn discover_runners(config_override: Option<&Path>) -> Vec<crate::shared::models::RunnerInfo> {
    let roots = crate::shared::config::Config::load(config_override)
        .ok()
        .map(|c| c.runner_roots)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(runners::discover_roots);
    runners::discover(&roots)
}

/// Every place the config might live, so uninstall finds it wherever `config`
/// wrote it: an explicit override, `$GHR_STATS_CONFIG`, the scope's file, and the
/// sudo-invoker's home (where `sudo ghr-stats config` lands it).
fn config_candidates(scope: Scope, config_override: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(p) = config_override {
        push(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("GHR_STATS_CONFIG") {
        push(PathBuf::from(p));
    }
    push(scope.config_file());
    push(paths::config_write_target(config_override));
    out
}

/// The data-domain files (database + WAL/SHM sidecars, event log, serve lock).
/// The IPC socket is deliberately NOT here: it lives on tmpfs under the unit's
/// RuntimeDirectory=, torn down by `systemd::uninstall` (the `service` domain),
/// not left in `data_dir`.
fn data_files(scope: Scope) -> Vec<PathBuf> {
    let db = scope.db_path();
    vec![
        db.clone(),
        db.with_extension("db-wal"),
        db.with_extension("db-shm"),
        scope.event_log(),
        scope.data_dir().join("serve.lock"),
    ]
}

/// Best-effort note when artifacts exist in the OTHER scope than the one we're
/// acting on — so a user-scope run doesn't silently ignore a system install.
fn cross_scope_probe(scope: Scope) -> Vec<String> {
    let other = match scope {
        Scope::User => Scope::System,
        Scope::System => Scope::User,
    };
    let hits = [
        other.config_file(),
        other.db_path(),
        other.bin_path(),
        other.systemd_unit_path(),
    ]
    .into_iter()
    .filter(|p| p.exists())
    .map(|p| p.display().to_string())
    .collect::<Vec<_>>();
    if hits.is_empty() {
        return Vec::new();
    }
    let re_run = match other {
        Scope::System => "sudo ghr-stats uninstall --system",
        Scope::User => "ghr-stats uninstall --user",
    };
    let mut lines: Vec<String> = hits;
    lines.push(format!("↳ to remove these, re-run: {re_run}"));
    lines
}

fn plan_line(rp: &RunnerHookPlan) -> String {
    match &rp.action {
        RevertAction::Leave { why } => format!("  · {} — {why}", rp.name),
        RevertAction::Manual { why } => format!("  ⚠ {} — {why}", rp.name),
        RevertAction::Strip { .. } => format!("  {} — remove ghr-stats hook (→ unset)", rp.name),
        RevertAction::Restore { originals, .. } => {
            format!(
                "  {} — restore your hook ({})",
                rp.name,
                originals.0.display()
            )
        }
    }
}

/// A single destructive-action confirm. Any read error (no TTY) fails safe to
/// "no" — a headless caller must pass `--yes`.
fn confirm() -> bool {
    dialoguer::Confirm::new()
        .with_prompt("Remove the above?")
        .default(false)
        .interact()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_action_installed_removes_even_if_cargo_exe() {
        let installed = Path::new("/usr/local/bin/ghr-stats");
        let cargo = Path::new("/home/u/.cargo/bin/ghr-stats");
        assert_eq!(
            binary_action(installed, true, Some(cargo)),
            BinaryAction::Remove(installed.to_path_buf())
        );
    }

    #[test]
    fn binary_action_cargo_build_instructs_not_deletes() {
        let installed = Path::new("/usr/local/bin/ghr-stats");
        let cargo = Path::new("/home/u/.cargo/bin/ghr-stats");
        assert_eq!(
            binary_action(installed, false, Some(cargo)),
            BinaryAction::InstructCargo(cargo.to_path_buf())
        );
        // Not installed + not cargo ⇒ nothing to do.
        assert_eq!(
            binary_action(installed, false, Some(Path::new("/opt/ghr-stats"))),
            BinaryAction::NotInstalled(installed.to_path_buf())
        );
    }

    #[test]
    fn is_cargo_bin_matches_cargo_dir_only() {
        assert!(is_cargo_bin(Path::new("/home/u/.cargo/bin/ghr-stats")));
        assert!(!is_cargo_bin(Path::new("/usr/local/bin/ghr-stats")));
        assert!(!is_cargo_bin(Path::new("/home/u/.local/bin/ghr-stats")));
    }

    #[test]
    fn domains_from_args_maps_positionals() {
        use crate::cli::UninstallDomain as D;
        let all = Domains::all();
        assert!(all.hooks && all.service && all.config && all.data && all.binary);
        // `all` positional expands to every domain.
        let a = UninstallArgs {
            domains: vec![D::All],
            yes: false,
            system: false,
            user: false,
        };
        let d = Domains::from_args(&a);
        assert!(d.hooks && d.service && d.config && d.data && d.binary);
        // A subset selects exactly those.
        let a = UninstallArgs {
            domains: vec![D::Config, D::Data],
            yes: false,
            system: false,
            user: false,
        };
        let d = Domains::from_args(&a);
        assert!(d.config && d.data);
        assert!(!d.hooks && !d.service && !d.binary);
    }
}
