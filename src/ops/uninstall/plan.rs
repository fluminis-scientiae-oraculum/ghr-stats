//! Phase one: DETECT + PLAN, read-only by construction.
//!
//! Everything here answers "what is installed, and what would removing it mean"
//! without touching a single file. That is what makes a bare `uninstall` a safe
//! dry-run: nothing in this module can delete, so the first phase cannot.
//!
//! Two of these probes are deliberately CONFIG-FREE. [`super::Plan::detect`]
//! reaches for runner roots through [`discover_runners`], which falls back to
//! systemd auto-detection when no config loads — so hooks can still be reverted
//! after the config is already gone. And [`config_candidates`] looks in every
//! place `config` might have written, including the sudo-invoker's home, because
//! an uninstall that cannot find the file it is meant to remove is not an
//! uninstall.
//!
//! [`cross_scope_probe`] exists so a user-scope run does not silently ignore a
//! system install; it reports, and never acts on, the other scope.

use std::path::{Path, PathBuf};

use crate::shared::collectors::runners;
use crate::shared::hooks::install;
use crate::shared::hooks::uninstall::{self as hook_revert};
use crate::shared::paths::{self, Scope};

use super::{ConfigItem, Domains, Plan};

/// What removing the binary means on this host. Pure result of [`binary_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BinaryAction {
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
pub(super) fn binary_action(
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
pub(super) fn is_cargo_bin(exe: &Path) -> bool {
    exe.parent().is_some_and(|p| p.ends_with(".cargo/bin"))
}

impl Plan {
    pub(super) fn detect(scope: Scope, domains: Domains, config_override: Option<&Path>) -> Self {
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
