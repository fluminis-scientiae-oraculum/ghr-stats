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
//!
//! Those two phases are the seam, and it is a SAFETY boundary rather than a
//! layer. [`plan`] holds everything the read-only phase does; [`apply`] holds
//! EVERY function in this module that can delete something. So "a dry-run removes
//! nothing" is now checkable by reading one file's imports rather than by
//! auditing every call site — `std::fs::remove_*` appears in [`apply`] and
//! nowhere else.
//!
//! This file keeps [`run`], the shapes both phases speak, and the receipt.
//! `Plan::render` takes an `execute: bool` precisely BECAUSE it serves both
//! phases — the dry-run and the real run print the same inventory, differing only
//! in tense — so it belongs above the cut rather than in either half. That
//! sameness is the feature: what you were shown is what gets removed.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::cli::{UninstallArgs, UninstallDomain};
use crate::ops::systemd;
use crate::shared::hooks::uninstall::{RevertAction, RunnerHookPlan};
use crate::shared::paths::Scope;
use crate::shared::privileged;

mod apply;
mod plan;

use plan::BinaryAction;

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
    use super::plan::{binary_action, is_cargo_bin};
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
