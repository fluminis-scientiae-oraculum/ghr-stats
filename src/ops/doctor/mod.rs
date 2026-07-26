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
//! rather than accepting the loaded [`Config`](crate::shared::config::Config).
//! That is deliberate: `Config::load` falls back to `Config::default()` when the
//! system file is unreadable, which is right for the dashboard (it can still
//! read persistent data over the socket) and catastrophic here — `doctor` would
//! report on a phantom config it never read, and every org and runner root would
//! look merely absent. Taking the path instead of the config makes that mistake
//! unrepresentable rather than merely avoided.
//!
//! # Layout
//!
//! The checks divide on *who can answer them*, which is also how their
//! dependencies divide: [`collector`] opens the socket and imports the wire,
//! [`host`] reads this machine and imports the config, the runner roots and the
//! PAT validator. Neither imports the other. What stays here is the part that is
//! neither — the report shape, the verdict over a set of checks, and the human
//! rendering — so a wire change and a config change land in different files and
//! neither disturbs the payload.

mod collector;
mod host;

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::ops::explain::Boundary;
use crate::shared::collectors::runners;
use crate::shared::models::Verdict;
use crate::shared::util::{BUILD_VERSION, now_epoch, to_rfc3339_utc};

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
    /// A name in this verb's closed vocabulary — `&'static str` for the same
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
    let source = host::load_config(config_path);
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
        .map(|c| host::org_names(c, &discovered))
        .unwrap_or_default();

    let mut checks = vec![host::config_check(&source, &orgs)];
    checks.extend(collector::collector_checks());
    checks.extend(host::config_dependent(&source, args, &discovered, &orgs));

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

fn skipped(id: &'static str, boundary: Boundary, why: &str) -> Check {
    Check {
        id,
        boundary,
        outcome: Outcome::Skipped {
            why: why.to_string(),
        },
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
}
