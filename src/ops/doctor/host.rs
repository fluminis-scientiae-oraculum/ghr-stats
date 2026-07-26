//! The checks this machine can answer for itself.
//!
//! No socket is opened in here. Every check reads the config, the runner install
//! dirs, the database file or a PAT, which is why one unreadable config takes
//! all of them out at once and why that reason is stated once, in
//! [`config_dependent`], rather than rediscovered per check.
//!
//! [`ConfigSource`] is the reason the file starts with provenance rather than a
//! [`Config`]: `Config::load` substitutes defaults when the system file is
//! unreadable, so a `doctor` handed a loaded config would report a phantom
//! install as merely empty. See the module docs on [`super`].

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::cli::DoctorArgs;
use crate::ops::explain::Boundary;
use crate::shared::collectors::runners;
use crate::shared::config::Config;
use crate::shared::github::validate::{self, Verdict as PatVerdict};
use crate::shared::hooks::install::{self, HookStatus};
use crate::shared::models::RunnerInfo;
use crate::shared::paths::{self, Scope};

use super::{Check, Outcome, skipped};

/// Where the config came from, or what stopped it.
///
/// `doctor` resolves this itself rather than accepting a loaded [`Config`],
/// because a `Config` carries no provenance and the fallback-to-defaults path is
/// silent. See the module docs.
pub(super) enum ConfigSource {
    Loaded {
        path: PathBuf,
        cfg: Box<Config>,
    },
    Unreadable {
        path: PathBuf,
        why: String,
    },
    Invalid {
        path: PathBuf,
        why: String,
    },
    /// No config anywhere; the path is where one WOULD be written.
    Missing {
        path: PathBuf,
    },
}

impl ConfigSource {
    /// The config, when there is one. Every config-dependent check goes through
    /// this, so "reasoned over a config we never loaded" has one place to be
    /// wrong instead of one per check.
    pub(super) fn cfg(&self) -> Option<&Config> {
        match self {
            ConfigSource::Loaded { cfg, .. } => Some(cfg),
            _ => None,
        }
    }

    /// Why the config-dependent checks cannot run. `None` when they can.
    fn blocked(&self) -> Option<String> {
        match self {
            ConfigSource::Loaded { .. } => None,
            ConfigSource::Unreadable { path, .. } => Some(format!(
                "the config at {} is not readable by this user — re-run with sudo",
                path.display()
            )),
            ConfigSource::Invalid { path, .. } => {
                Some(format!("the config at {} does not parse", path.display()))
            }
            ConfigSource::Missing { .. } => Some("there is no config to read".to_string()),
        }
    }
}

/// Read the config with its provenance intact.
pub(super) fn load_config(explicit: Option<&Path>) -> ConfigSource {
    let Some(path) = paths::resolve_config(explicit) else {
        return ConfigSource::Missing {
            path: paths::config_write_target(explicit),
        };
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => ConfigSource::Loaded {
                path,
                cfg: Box::new(cfg),
            },
            Err(e) => ConfigSource::Invalid {
                path,
                why: e.to_string(),
            },
        },
        Err(e) => ConfigSource::Unreadable {
            path,
            why: e.to_string(),
        },
    }
}

/// Does a config exist, and does it parse? Pure given the source.
pub(super) fn config_check(source: &ConfigSource, orgs: &[String]) -> Check {
    let outcome = match source {
        ConfigSource::Loaded { path, cfg } => Outcome::Pass {
            detail: format!(
                "{} — {} org(s), {} runner root(s), api every {}s",
                path.display(),
                orgs.len(),
                cfg.runner_roots.len(),
                cfg.intervals.api_secs
            ),
        },
        // Not a failure: a non-root operator reading a root-owned config is the
        // expected shape of a system deployment, not a broken install. It is a
        // SKIP, which still refuses to certify what it could not see.
        ConfigSource::Unreadable { path, why } => Outcome::Skipped {
            why: format!("{}: {why} — re-run with sudo", path.display()),
        },
        ConfigSource::Invalid { path, why } => Outcome::Fail {
            detail: format!("{}: {why}", path.display()),
            fix: format!(
                "fix the syntax, or re-run `ghr-stats config` to rewrite {}",
                path.display()
            ),
        },
        ConfigSource::Missing { path } => Outcome::Fail {
            detail: format!("no config file at {}", path.display()),
            fix: "run `sudo ghr-stats config` to create one".to_string(),
        },
    };
    Check {
        id: "config",
        boundary: Boundary::Config,
        outcome,
    }
}

/// The checks that need a config we could actually read. When we could not, each
/// one is skipped with the SAME reason — stated once, here, rather than
/// rediscovered per check.
pub(super) fn config_dependent(
    source: &ConfigSource,
    args: &DoctorArgs,
    discovered: &[RunnerInfo],
    orgs: &[String],
) -> Vec<Check> {
    let ids = [
        ("runner-roots", Boundary::Local),
        ("database", Boundary::Local),
        ("hooks", Boundary::Local),
        ("tokens", Boundary::Config),
    ];
    let Some(cfg) = source.cfg() else {
        let why = source
            .blocked()
            .unwrap_or_else(|| "the config is unavailable".to_string());
        return ids
            .into_iter()
            .map(|(id, b)| skipped(id, b, &why))
            .collect();
    };

    vec![
        runner_roots_check(cfg, discovered.len()),
        database_check(cfg),
        hooks_check(discovered),
        tokens_check(cfg, discovered, orgs, args.offline),
    ]
}

/// Are there roots, and did they yield runners?
fn runner_roots_check(cfg: &Config, found: usize) -> Check {
    let roots = runners::effective_roots(&cfg.runner_roots);
    let outcome = if roots.is_empty() {
        Outcome::Fail {
            detail: "no runner roots configured and none discoverable".to_string(),
            fix: "run `sudo ghr-stats config` to set `runner_roots`".to_string(),
        }
    } else if found == 0 {
        Outcome::Fail {
            detail: format!(
                "{} root(s) scanned, no runner install dirs found (a runner dir contains a \
                 `.runner` file)",
                roots.len()
            ),
            fix: "check `runner_roots` points at the parent of the install dirs, and that this \
                  user can read them"
                .to_string(),
        }
    } else {
        Outcome::Pass {
            detail: format!("{found} runner(s) across {} root(s)", roots.len()),
        }
    };
    Check {
        id: "runner-roots",
        boundary: Boundary::Local,
        outcome,
    }
}

/// The database file: present, and how big. Writability is the collector's
/// problem — it runs as root and is the only writer — so this reports what a
/// reader can establish without claiming more.
fn database_check(cfg: &Config) -> Check {
    let outcome = match std::fs::metadata(&cfg.db_path) {
        Ok(m) => Outcome::Pass {
            detail: format!("{} — {}", cfg.db_path.display(), human_bytes(m.len())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::Fail {
            detail: format!("no database at {}", cfg.db_path.display()),
            fix: "start the collector (`sudo systemctl start ghr-stats.service`); it creates \
                  and migrates the database on first run"
                .to_string(),
        },
        Err(e) => Outcome::Skipped {
            why: format!("{}: {e}", cfg.db_path.display()),
        },
    };
    Check {
        id: "database",
        boundary: Boundary::Local,
        outcome,
    }
}

/// Hook install state per runner. Job-level data exists only if these are wired,
/// so an unhooked fleet is a real gap — silent, which is why it is checked.
fn hooks_check(discovered: &[RunnerInfo]) -> Check {
    let our_dirs = [
        install::hooks_dir(&Scope::System.data_dir()),
        install::hooks_dir(&Scope::User.data_dir()),
    ];
    let mut ours = 0usize;
    let mut foreign = Vec::new();
    let mut unset = Vec::new();
    let mut unreadable = Vec::new();
    for r in discovered {
        match install::detect_in(&r.dir, &our_dirs) {
            HookStatus::Ours => ours += 1,
            HookStatus::Foreign => foreign.push(r.name.clone()),
            HookStatus::Unset => unset.push(r.name.clone()),
            HookStatus::Unreadable => unreadable.push(r.name.clone()),
        }
    }
    let outcome = if discovered.is_empty() {
        Outcome::Skipped {
            why: "no runners discovered to check".to_string(),
        }
    } else if !unset.is_empty() || !foreign.is_empty() {
        let mut detail = format!("{ours}/{} runners hooked", discovered.len());
        if !unset.is_empty() {
            detail.push_str(&format!("; no hooks: {}", unset.join(", ")));
        }
        if !foreign.is_empty() {
            detail.push_str(&format!("; foreign hooks: {}", foreign.join(", ")));
        }
        Outcome::Fail {
            detail,
            fix: "run `sudo ghr-stats config` and install hooks; without them there is no \
                  per-job data"
                .to_string(),
        }
    } else if !unreadable.is_empty() {
        Outcome::Skipped {
            why: format!(
                "cannot read the `.env` of {} — re-run with sudo",
                unreadable.join(", ")
            ),
        }
    } else {
        Outcome::Pass {
            detail: format!("{ours}/{} runners hooked", discovered.len()),
        }
    };
    Check {
        id: "hooks",
        boundary: Boundary::Local,
        outcome,
    }
}

/// Per-org PAT validation.
///
/// The validation available is a read-and-confirm, not a scope listing: GitHub
/// exposes no bearer-side introspection of a fine-grained token's granted
/// permissions, so `validate` proves the token can list the org's runners and
/// how many of them match locally-discovered ones. That IS the check that
/// matters — a token that cannot list runners is the failure, whatever its
/// scopes claim.
///
/// This is the only check that touches the network; `--offline` skips it, and
/// skipping keeps the verdict at "cannot determine" rather than green.
fn tokens_check(cfg: &Config, discovered: &[RunnerInfo], orgs: &[String], offline: bool) -> Check {
    let check = Check {
        id: "tokens",
        boundary: Boundary::Config,
        outcome: Outcome::Pass {
            detail: String::new(),
        },
    };
    if orgs.is_empty() {
        return Check {
            outcome: Outcome::Fail {
                detail: "no orgs configured and none discovered from `.runner` files".to_string(),
                fix: "run `sudo ghr-stats config` to add an org and its PAT".to_string(),
            },
            ..check
        };
    }
    if offline {
        return Check {
            outcome: Outcome::Skipped {
                why: "--offline: PAT validation is the one check that calls GitHub".to_string(),
            },
            ..check
        };
    }

    let local_ids: HashSet<i64> = discovered.iter().map(|r| r.agent_id).collect();
    let mut ok = Vec::new();
    let mut missing = Vec::new();
    let mut rejected = Vec::new();
    for org in orgs {
        let Some(token) = cfg.github_token_for(org) else {
            missing.push(org.clone());
            continue;
        };
        match validate::validate(&token, org, &local_ids) {
            PatVerdict::Valid {
                runners, matched, ..
            } => ok.push(format!("{org} ({matched}/{runners} runners confirmed)")),
            // The reason is the API's own — never the token.
            PatVerdict::Rejected(why) => rejected.push(format!("{org}: {why}")),
        }
    }

    let outcome = if !rejected.is_empty() {
        Outcome::Fail {
            detail: rejected.join("; "),
            fix: "run `sudo ghr-stats config` to replace the org's PAT (fine-grained, \
                  Organization → Self-hosted runners: Read)"
                .to_string(),
        }
    } else {
        let mut detail = format!("{} PAT(s) validated: {}", ok.len(), ok.join(", "));
        if !missing.is_empty() {
            detail.push_str(&format!(
                "; no PAT: {} (never reconciled — expected for an account with no org runner API)",
                missing.join(", ")
            ));
        }
        Outcome::Pass { detail }
    };
    Check { outcome, ..check }
}

/// Every org this host cares about: configured, plus discovered from `.runner`
/// files. A configured org with no runners still needs a working PAT, and a
/// discovered org with no config entry is exactly the gap worth reporting.
pub(super) fn org_names(cfg: &Config, discovered: &[RunnerInfo]) -> Vec<String> {
    let mut orgs: BTreeSet<String> = cfg.orgs.iter().cloned().collect();
    orgs.extend(discovered.iter().map(|r| r.org.clone()));
    orgs.into_iter().collect()
}

/// Bytes at human scale — a database size is read by a person deciding whether
/// to prune, not parsed.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unreadable config is a SKIP, not a failure — a non-root operator
    /// reading a root-owned config is the expected shape of a system
    /// deployment. It still refuses to certify anything it could not see.
    #[test]
    fn an_unreadable_config_skips_rather_than_fails() {
        let source = ConfigSource::Unreadable {
            path: PathBuf::from("/etc/ghr-stats/config.toml"),
            why: "Permission denied (os error 13)".to_string(),
        };
        let c = config_check(&source, &[]);
        assert!(matches!(c.outcome, Outcome::Skipped { .. }));
        assert!(source.blocked().unwrap().contains("sudo"));
    }

    /// ...and every config-dependent check inherits that same reason, rather
    /// than each rediscovering it or, worse, reading `Config::default()` and
    /// reporting a phantom fleet as merely empty.
    #[test]
    fn config_dependent_checks_are_skipped_together_with_one_reason() {
        let source = ConfigSource::Unreadable {
            path: PathBuf::from("/etc/ghr-stats/config.toml"),
            why: "Permission denied (os error 13)".to_string(),
        };
        let args = DoctorArgs {
            json: false,
            offline: true,
        };
        let checks = config_dependent(&source, &args, &[], &[]);
        assert_eq!(checks.len(), 4);
        for c in &checks {
            match &c.outcome {
                Outcome::Skipped { why } => assert!(why.contains("re-run with sudo"), "{why}"),
                other => panic!("{}: expected a skip, got {other:?}", c.id),
            }
        }
    }

    #[test]
    fn a_missing_config_fails_with_the_command_that_creates_one() {
        let c = config_check(
            &ConfigSource::Missing {
                path: PathBuf::from("/etc/ghr-stats/config.toml"),
            },
            &[],
        );
        match c.outcome {
            Outcome::Fail { fix, .. } => assert!(fix.contains("ghr-stats config"), "{fix}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn bytes_render_at_human_scale() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1_213_259_776), "1.1 GiB");
    }
}
