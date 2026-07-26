//! `ghr-stats doctor` — one preflight, in the order a fault propagates.
//!
//! `status` says whether the fleet is healthy, `explain` says why it isn't, and
//! `timeline` says when it stopped being. All three assume the tool itself is
//! wired up correctly. This is the verb for when that assumption is the thing in
//! doubt: config, PATs, hooks, socket, database, and the version drift between a
//! binary and the collector it is talking to.
//!
//! Two properties are load-bearing.
//!
//! **A check that could not run is not a check that passed.** [`Outcome`] has a
//! third arm for exactly this, and any skip drags the verdict to
//! [`Verdict::Unknown`] rather than leaving it green. A preflight that certifies
//! what it never looked at is worse than no preflight, because it is trusted.
//! The common case is real: the system config is `0600 root`, so a non-root
//! `doctor` genuinely cannot inspect PATs, and must say so.
//!
//! **Every failure ships its fix.** [`Outcome::Fail`] cannot be constructed
//! without one. A failing check with no remedy is an error message, and the
//! caller already had one of those.
//!
//! Unlike the other verbs this one loads the config *itself*, from the path,
//! rather than accepting the loaded [`Config`]. That is deliberate:
//! [`Config::load`] falls back to `Config::default()` when the system file is
//! unreadable, which is right for the dashboard (it can still read persistent
//! data over the socket) and catastrophic here — `doctor` would report on a
//! phantom config it never read, and every org and runner root would look
//! merely absent. Taking the path instead of the config makes that mistake
//! unrepresentable rather than merely avoided.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::ops::explain::Boundary;
use crate::shared::collectors::runners;
use crate::shared::config::Config;
use crate::shared::github::validate::{self, Verdict as PatVerdict};
use crate::shared::hooks::install::{self, HookStatus};
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{self, Query, Request, Response};
use crate::shared::models::timeline::TimelineQuery;
use crate::shared::models::{FleetStatus, Verdict};
use crate::shared::paths::{self, Scope};
use crate::shared::util::{BUILD_VERSION, now_epoch, to_rfc3339_utc};

/// How far back `doctor` looks when asking where the record starts. Only the
/// window's `truncated_at` is read, so the limit is 1: this asks a question
/// about retention, not for history.
const HISTORY_PROBE_SECS: i64 = 7 * 86_400;

/// What one check found.
///
/// Three arms, not two: "could not check" is a distinct answer from "checked and
/// it was fine", and collapsing them is how a preflight starts lying. `Fail`
/// carries its `fix` in the same variant, so a failure without a remedy cannot
/// be constructed.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Pass {
        detail: String,
    },
    Fail {
        detail: String,
        /// The single next action. Concrete enough to paste.
        fix: String,
    },
    Skipped {
        /// What stopped us looking — never a restatement of the check's name.
        why: String,
    },
}

/// One preflight check.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Check {
    /// A name in this file's closed vocabulary — `&'static str` for the same
    /// reason `explain`'s finding ids are: it cannot be typo'd into existence.
    pub id: &'static str,
    /// Which side of the fence this check speaks for. The same taxonomy
    /// `explain` emits, so an agent reading both verbs reads one vocabulary.
    pub boundary: Boundary,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The `doctor` payload.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    /// The binary that ran the checks — half of every version comparison below.
    pub binary_version: &'static str,
    pub verdict: Verdict,
    pub checks: Vec<Check>,
}

/// Where the config came from, or what stopped it.
///
/// `doctor` resolves this itself rather than accepting a loaded [`Config`],
/// because a `Config` carries no provenance and the fallback-to-defaults path is
/// silent. See the module docs.
enum ConfigSource {
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
    fn cfg(&self) -> Option<&Config> {
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

/// Run the verb.
pub fn run(args: &DoctorArgs, config_path: Option<&Path>) -> Result<Verdict> {
    let report = diagnose(args, config_path);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", human(&report));
    }
    Ok(report.verdict)
}

/// Gather every check and score them.
fn diagnose(args: &DoctorArgs, config_path: Option<&Path>) -> Report {
    let source = load_config(config_path);
    let now = now_epoch();

    // Discovery walks every runner install dir and reads its `.runner`, so it
    // happens ONCE here and is handed to each check that needs it. Letting the
    // checks call it themselves cost three full scans of the fleet for one
    // invocation — invisible in a unit test, obvious the first time it ran on a
    // host with 21 runners.
    let discovered = source
        .cfg()
        .map(|c| runners::discover(&runners::effective_roots(&c.runner_roots)))
        .unwrap_or_default();
    let orgs = source
        .cfg()
        .map(|c| org_names(c, &discovered))
        .unwrap_or_default();

    let mut checks = vec![config_check(&source, &orgs)];
    checks.extend(collector_checks());
    checks.extend(config_dependent(&source, args, &discovered, &orgs));

    Report {
        schema_version: 1,
        generated_at: to_rfc3339_utc(now),
        generated_at_epoch: now,
        binary_version: BUILD_VERSION,
        verdict: verdict_of(&checks),
        checks,
    }
}

/// The verdict over a set of checks. Pure.
///
/// A skip outranks a pass but not a failure: "something is broken" is more
/// actionable than "I could not tell", and a caller branching on the exit code
/// should reach the worse news first. It never returns [`Verdict::Ok`] while any
/// check was skipped — that is the whole point of the third arm.
fn verdict_of(checks: &[Check]) -> Verdict {
    if checks
        .iter()
        .any(|c| matches!(c.outcome, Outcome::Fail { .. }))
    {
        Verdict::Degraded
    } else if checks
        .iter()
        .any(|c| matches!(c.outcome, Outcome::Skipped { .. }))
    {
        Verdict::Unknown
    } else {
        Verdict::Ok
    }
}

/// Read the config with its provenance intact.
fn load_config(explicit: Option<&Path>) -> ConfigSource {
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
fn config_check(source: &ConfigSource, orgs: &[String]) -> Check {
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

/// Everything the collector can answer for: that it is there, that it speaks our
/// wire version, and — if it does — what it knows about reconciles and history.
fn collector_checks() -> Vec<Check> {
    let mut client = match Client::connect_any() {
        Ok(c) => c,
        Err(reason) => {
            let (detail, fix) = match reason {
                crate::shared::ipc::client::EphemeralReason::VersionDrift { server } => (
                    format!(
                        "the collector speaks wire v{server}, this binary speaks v{} — almost \
                         always a binary upgraded without restarting the service",
                        ipc::VERSION
                    ),
                    "sudo systemctl restart ghr-stats.service".to_string(),
                ),
                other => (
                    format!("no usable collector on the socket ({})", other.word()),
                    "check `systemctl status ghr-stats.service` and \
                     `journalctl -u ghr-stats -n 50`"
                        .to_string(),
                ),
            };
            return vec![
                Check {
                    id: "collector",
                    boundary: Boundary::Local,
                    outcome: Outcome::Fail { detail, fix },
                },
                skipped("reconcile", Boundary::Github, "no collector answered"),
                skipped("history", Boundary::Local, "no collector answered"),
            ];
        }
    };

    let version = client.collector_version().unwrap_or("unknown").to_string();
    let mut checks = vec![Check {
        id: "collector",
        boundary: Boundary::Local,
        outcome: version_outcome(&version),
    }];

    match client.request(&Request::Query(Query::FleetStatus)) {
        Ok(Response::FleetStatus(s)) => checks.push(reconcile_check(&s)),
        // It handshook and then refused the query — its own fault, not an
        // absence, and the distinction is what tells "install one" from
        // "look at the one you have".
        _ => checks.push(Check {
            id: "reconcile",
            boundary: Boundary::Local,
            outcome: Outcome::Fail {
                detail: "the collector is up but did not answer a status query".to_string(),
                fix: "check `journalctl -u ghr-stats -n 50` for a database error".to_string(),
            },
        }),
    }
    checks.push(history_check(&mut client));
    checks
}

/// Binary semver against the collector's. Pure.
///
/// A wire mismatch never reaches here — the handshake refuses first — so this
/// catches the quieter case: same wire version, different build. That is a
/// restart that has not happened yet, and it is worth naming before it becomes a
/// wire break at the next bump.
fn version_outcome(collector: &str) -> Outcome {
    if collector == BUILD_VERSION {
        Outcome::Pass {
            detail: format!("v{collector}, wire v{}", ipc::VERSION),
        }
    } else {
        Outcome::Fail {
            detail: format!(
                "the collector runs v{collector} but this binary is v{BUILD_VERSION} — they \
                 still share wire v{}, so nothing has broken yet",
                ipc::VERSION
            ),
            fix: "sudo systemctl restart ghr-stats.service".to_string(),
        }
    }
}

/// Per-org reconcile freshness, from the snapshot the collector already
/// assembles. Pure.
///
/// An org that has NEVER reconciled is reported, not failed: this host has one
/// that never can (a personal account exposes no org runner API), and a check
/// that cries wolf on every run is a check the operator learns to skip. An org
/// that reconciled once and then stopped is the real fault.
fn reconcile_check(s: &FleetStatus) -> Check {
    let mut stale = Vec::new();
    let mut never = Vec::new();
    let mut ok = 0usize;
    for o in &s.orgs {
        match o.reconcile_age_s {
            None => never.push(o.org.clone()),
            Some(age) if o.verdict == Verdict::Ok => {
                let _ = age;
                ok += 1;
            }
            Some(age) => stale.push(format!("{} ({age}s ago)", o.org)),
        }
    }
    let outcome = if stale.is_empty() {
        let mut detail = format!("{ok} org(s) reconciling");
        if !never.is_empty() {
            detail.push_str(&format!(
                "; never reconciled: {} (expected for an account with no org runner API)",
                never.join(", ")
            ));
        }
        Outcome::Pass { detail }
    } else {
        Outcome::Fail {
            detail: format!("stale reconcile: {}", stale.join(", ")),
            fix: "check the org's PAT with `sudo ghr-stats doctor`, then \
                  `journalctl -u ghr-stats | grep reconcile`"
                .to_string(),
        }
    };
    Check {
        id: "reconcile",
        boundary: Boundary::Github,
        outcome,
    }
}

/// Where the retained record starts.
///
/// Asked through the existing `timeline` query rather than a new one: its window
/// already reports `truncated_at` — where data begins when it begins after the
/// window opens — which is exactly the retention question. Pruning is manual, so
/// this reports rather than judges.
fn history_check(client: &mut Client) -> Check {
    let query = TimelineQuery {
        since_ts: now_epoch() - HISTORY_PROBE_SECS,
        limit: 1,
        org: None,
        runner: None,
        samples: false,
    };
    let outcome = match client.request(&Request::Query(Query::Timeline(query))) {
        Ok(Response::Timeline(t)) => Outcome::Pass {
            detail: match t.window.truncated_at {
                Some(first) => format!(
                    "the record starts {} — pruning is manual (`ghr-stats db prune --days N`)",
                    to_rfc3339_utc(first)
                ),
                None => format!(
                    "the record covers the full {}d probe — pruning is manual \
                     (`ghr-stats db prune --days N`)",
                    HISTORY_PROBE_SECS / 86_400
                ),
            },
        },
        _ => Outcome::Skipped {
            why: "the collector did not answer a history query".to_string(),
        },
    };
    Check {
        id: "history",
        boundary: Boundary::Local,
        outcome,
    }
}

/// The checks that need a config we could actually read. When we could not, each
/// one is skipped with the SAME reason — stated once, here, rather than
/// rediscovered per check.
fn config_dependent(
    source: &ConfigSource,
    args: &DoctorArgs,
    discovered: &[crate::shared::models::RunnerInfo],
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
fn hooks_check(discovered: &[crate::shared::models::RunnerInfo]) -> Check {
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
fn tokens_check(
    cfg: &Config,
    discovered: &[crate::shared::models::RunnerInfo],
    orgs: &[String],
    offline: bool,
) -> Check {
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
fn org_names(cfg: &Config, discovered: &[crate::shared::models::RunnerInfo]) -> Vec<String> {
    let mut orgs: BTreeSet<String> = cfg.orgs.iter().cloned().collect();
    orgs.extend(discovered.iter().map(|r| r.org.clone()));
    orgs.into_iter().collect()
}

fn skipped(id: &'static str, boundary: Boundary, why: &str) -> Check {
    Check {
        id,
        boundary,
        outcome: Outcome::Skipped {
            why: why.to_string(),
        },
    }
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

/// The human rendering: one line per check, failures carrying their fix
/// underneath. Pure, so the whole layout is testable without a fleet.
fn human(r: &Report) -> String {
    let mut out = format!("ghr-stats doctor — binary v{}\n\n", r.binary_version);
    // One unreadable config skips four checks for the same reason, and four
    // identical 80-column lines bury the one line that isn't. The JSON keeps
    // every reason in full — a machine reads one check at a time and cannot
    // look up.
    let mut last_skip: Option<&str> = None;
    for c in &r.checks {
        let (tag, detail) = match &c.outcome {
            Outcome::Pass { detail } => ("ok", detail.as_str()),
            Outcome::Fail { detail, .. } => ("FAIL", detail.as_str()),
            Outcome::Skipped { why } => (
                "skipped",
                if last_skip == Some(why.as_str()) {
                    "(same reason as above)"
                } else {
                    why.as_str()
                },
            ),
        };
        last_skip = match &c.outcome {
            Outcome::Skipped { why } => Some(why.as_str()),
            _ => None,
        };
        out.push_str(&format!("  {tag:<8} {:<13} {detail}\n", c.id));
        if let Outcome::Fail { fix, .. } = &c.outcome {
            out.push_str(&format!("  {:<8} {:<13} fix: {fix}\n", "", ""));
        }
    }
    let failed = r
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Fail { .. }))
        .count();
    let skipped = r
        .checks
        .iter()
        .filter(|c| matches!(c.outcome, Outcome::Skipped { .. }))
        .count();
    out.push_str(&format!(
        "\nverdict: {} ({} check(s), {failed} failed, {skipped} skipped)\n",
        match r.verdict {
            Verdict::Ok => "ok",
            Verdict::Degraded => "degraded",
            Verdict::Unknown => "cannot determine",
        },
        r.checks.len()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(id: &'static str) -> Check {
        Check {
            id,
            boundary: Boundary::Local,
            outcome: Outcome::Pass {
                detail: "fine".to_string(),
            },
        }
    }
    fn fail(id: &'static str) -> Check {
        Check {
            id,
            boundary: Boundary::Local,
            outcome: Outcome::Fail {
                detail: "broken".to_string(),
                fix: "do the thing".to_string(),
            },
        }
    }

    /// The property the whole verb rests on: a check that could not run must
    /// never leave the verdict green. A preflight that certifies what it never
    /// looked at is worse than none, because it is trusted.
    #[test]
    fn a_skipped_check_is_never_reported_as_healthy() {
        let checks = vec![pass("a"), skipped("b", Boundary::Config, "no perms")];
        assert_eq!(verdict_of(&checks), Verdict::Unknown);
        assert_eq!(Verdict::Unknown.exit_code(), 2);
    }

    /// A failure outranks a skip: "something is broken" is more actionable than
    /// "I could not tell", and the caller should reach the worse news first.
    #[test]
    fn a_failure_outranks_a_skip() {
        let checks = vec![
            pass("a"),
            skipped("b", Boundary::Config, "no perms"),
            fail("c"),
        ];
        assert_eq!(verdict_of(&checks), Verdict::Degraded);
    }

    #[test]
    fn all_passing_is_ok() {
        assert_eq!(verdict_of(&[pass("a"), pass("b")]), Verdict::Ok);
    }

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

    /// The drift that is live on this host right now, in its quieter form: same
    /// wire version, different build. Naming it before the next wire bump is the
    /// point — after the bump it stops being a warning and starts being a
    /// refused handshake.
    #[test]
    fn a_collector_on_a_different_build_fails_with_the_restart() {
        match version_outcome("0.0.1") {
            Outcome::Fail { detail, fix } => {
                assert!(
                    detail.contains("0.0.1") && detail.contains(BUILD_VERSION),
                    "{detail}"
                );
                assert!(fix.contains("systemctl restart"), "{fix}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(matches!(
            version_outcome(BUILD_VERSION),
            Outcome::Pass { .. }
        ));
    }

    /// An org that has never reconciled is reported, not failed: this host has
    /// one that never can. A check that cries wolf every run is a check the
    /// operator learns to skip.
    #[test]
    fn an_org_that_never_reconciled_is_reported_not_failed() {
        use crate::shared::models::{FleetCounts, Mode, OrgStatus};
        let s = FleetStatus {
            schema_version: 1,
            generated_at: "now".to_string(),
            generated_at_epoch: 0,
            mode: Mode::Persistent,
            verdict: Verdict::Ok,
            fleet: FleetCounts {
                runners: 0,
                busy: 0,
                idle: 0,
                offline: 0,
                divergent: 0,
            },
            orgs: vec![
                OrgStatus {
                    org: "reconciling".to_string(),
                    runners: 1,
                    github_online: 1,
                    reconcile_age_s: Some(30),
                    verdict: Verdict::Ok,
                },
                OrgStatus {
                    org: "personal".to_string(),
                    runners: 1,
                    github_online: 0,
                    reconcile_age_s: None,
                    verdict: Verdict::Ok,
                },
            ],
            runners: Vec::new(),
        };
        match reconcile_check(&s).outcome {
            Outcome::Pass { detail } => {
                assert!(detail.contains("1 org(s) reconciling"), "{detail}");
                assert!(detail.contains("personal"), "{detail}");
            }
            other => panic!("expected a pass, got {other:?}"),
        }
    }

    /// Every failure ships an action. The type makes a fix mandatory; this
    /// pins that no arm slipped an empty string past it.
    #[test]
    fn every_failure_carries_a_non_empty_fix() {
        let checks = vec![
            config_check(
                &ConfigSource::Missing {
                    path: PathBuf::from("/etc/ghr-stats/config.toml"),
                },
                &[],
            ),
            Check {
                id: "collector",
                boundary: Boundary::Local,
                outcome: version_outcome("0.0.1"),
            },
        ];
        for c in &checks {
            if let Outcome::Fail { fix, .. } = &c.outcome {
                assert!(!fix.trim().is_empty(), "{} has an empty fix", c.id);
            }
        }
    }

    #[test]
    fn human_output_puts_the_fix_under_the_failure() {
        let r = Report {
            schema_version: 1,
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            generated_at_epoch: 0,
            binary_version: BUILD_VERSION,
            verdict: Verdict::Degraded,
            checks: vec![pass("config"), fail("collector")],
        };
        let out = human(&r);
        let lines: Vec<&str> = out.lines().collect();
        let idx = lines.iter().position(|l| l.contains("FAIL")).unwrap();
        assert!(lines[idx + 1].contains("fix: do the thing"), "{out}");
        assert!(
            out.contains("verdict: degraded (2 check(s), 1 failed, 0 skipped)"),
            "{out}"
        );
    }

    #[test]
    fn bytes_render_at_human_scale() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1_213_259_776), "1.1 GiB");
    }
}
