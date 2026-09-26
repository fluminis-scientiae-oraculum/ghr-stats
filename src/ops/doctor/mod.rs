//! `ghr-stats doctor`. Takes the config path, not a loaded config:
//! `Config::load` falls back to `Config::default()` when the system file is
//! unreadable, and doctor would then report on a config it never read.

mod collector;
mod host;

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::cli::DoctorArgs;
use crate::ops::explain::Boundary;
use crate::shared::collectors::runners;
use crate::shared::config::Intervals;
use crate::shared::models::Verdict;
use crate::shared::util::{BUILD_VERSION, now_epoch, to_rfc3339_utc};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Pass {
        detail: String,
    },
    Fail {
        detail: String,
        /// The single next action, concrete enough to paste.
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
    pub id: &'static str,
    pub boundary: Boundary,
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    pub binary_version: &'static str,
    pub verdict: Verdict,
    pub checks: Vec<Check>,
}

pub fn run(args: &DoctorArgs, config_path: Option<&Path>) -> Result<Verdict> {
    let report = diagnose(args, config_path);
    if args.json {
        crate::ops::emit_json(&report)?;
    } else {
        crate::ops::emit(&human(&report))?;
    }
    Ok(report.verdict)
}

fn diagnose(args: &DoctorArgs, config_path: Option<&Path>) -> Report {
    let source = host::load_config(config_path);
    let now = now_epoch();

    // Discovery reads every runner's `.runner`: do it once and share it.
    let discovered = source
        .cfg()
        .map(|c| runners::discover(&runners::effective_roots(&c.runner_roots)))
        .unwrap_or_default();
    let orgs = source
        .cfg()
        .map(|c| host::org_names(c, &discovered))
        .unwrap_or_default();

    let mut checks = vec![host::config_check(&source, &orgs)];
    let max_age = source.cfg().map_or_else(
        || Intervals::default().api_max_age(),
        |c| c.intervals.api_max_age(),
    );
    checks.extend(collector::collector_checks(max_age));
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

fn human(r: &Report) -> String {
    let mut out = format!("ghr-stats doctor — binary v{}\n\n", r.binary_version);
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

    #[test]
    fn a_skipped_check_is_never_reported_as_healthy() {
        let checks = vec![pass("a"), skipped("b", Boundary::Config, "no perms")];
        assert_eq!(verdict_of(&checks), Verdict::Unknown);
        assert_eq!(Verdict::Unknown.exit_code(), 2);
    }

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
