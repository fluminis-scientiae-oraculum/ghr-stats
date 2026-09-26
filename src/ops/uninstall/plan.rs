//! Detect + plan. Read-only: nothing here can delete.

use std::path::{Path, PathBuf};

use super::hooks::{self as hook_revert};
use crate::shared::collectors::runners;
use crate::shared::hooks::install;
use crate::shared::paths::Scope;

use super::{ConfigItem, Domains, Plan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BinaryAction {
    /// A `systemd install` copy; safe to remove even while running.
    Remove(PathBuf),
    /// A `cargo install` build: Cargo owns it, so print `cargo uninstall`.
    InstructCargo(PathBuf),
    NotInstalled(PathBuf),
}

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

/// Falls back to systemd auto-detection, so hooks can be reverted after the
/// config is gone.
fn discover_runners(config_override: Option<&Path>) -> Vec<crate::shared::models::RunnerInfo> {
    let roots = crate::shared::config::Config::load(config_override)
        .ok()
        .map(|c| c.runner_roots)
        .filter(|r| !r.is_empty())
        .unwrap_or_else(runners::discover_roots);
    runners::discover(&roots)
}

/// The explicit config, then this scope's own file; never another scope's.
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
    out
}

/// Not the IPC socket: it lives under RuntimeDirectory= and goes with the service.
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
        Scope::System => crate::shared::privileged::sudo_hint("uninstall --system"),
        Scope::User => format!("{} uninstall --user", crate::shared::privileged::exe_path()),
    };
    let mut lines: Vec<String> = hits;
    lines.push(format!("↳ to remove these, re-run: {re_run}"));
    lines
}
