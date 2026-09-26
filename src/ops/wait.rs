//! `ghr-stats wait`: block until every runner in scope is online to GitHub.
//! Polls at `intervals.local_secs`; faster only re-reads an unchanged sample.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::cli::WaitArgs;
use crate::ops::status::{Snapshot, Source};
use crate::shared::config::Config;
use crate::shared::models::FleetStatus;

pub(crate) enum Outcome {
    Held,
    /// The deadline passed while the GitHub view was readable.
    TimedOut,
    /// No collector, an empty filter, or a GitHub view unreadable at the deadline.
    Undetermined,
}

impl From<Outcome> for ExitCode {
    fn from(o: Outcome) -> Self {
        ExitCode::from(match o {
            Outcome::Held => 0,
            Outcome::TimedOut => 1,
            Outcome::Undetermined => 2,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    Held,
    NotYet { pending: usize, total: usize },
    Blind { unknown: usize, total: usize },
    Unanswerable(String),
}

pub fn run(args: &WaitArgs, cfg: &Config) -> Result<Outcome> {
    let interval = Duration::from_secs(cfg.intervals.local_secs.max(1));
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(args.timeout))
        .ok_or_else(|| anyhow::anyhow!("--timeout {} is too large", args.timeout))?;

    let mut last: Option<Progress> = None;
    let mut first = true;

    loop {
        let started = Instant::now();
        let snap = crate::ops::status::snapshot(cfg);
        let progress = evaluate(&snap, args.org.as_deref());

        match &progress {
            Progress::Held => {
                report(args, &snap.status)?;
                return Ok(Outcome::Held);
            }
            // Fatal only on the first poll: later it is likely a collector
            // restart the next poll resolves.
            Progress::Unanswerable(why) if first => {
                eprintln!("cannot wait: {why}");
                report(args, &snap.status)?;
                return Ok(Outcome::Undetermined);
            }
            Progress::Unanswerable(_) | Progress::Blind { .. } | Progress::NotYet { .. } => {}
        }
        let blind = matches!(progress, Progress::Blind { .. } | Progress::Unanswerable(_));
        if last.as_ref() != Some(&progress) {
            eprintln!("{}", describe(&progress));
            last = Some(progress);
        }
        first = false;

        // Checked after evaluating, so `--timeout 0` is a single evaluation.
        let now = Instant::now();
        if now >= deadline {
            report(args, &snap.status)?;
            return Ok(if blind {
                Outcome::Undetermined
            } else {
                Outcome::TimedOut
            });
        }
        std::thread::sleep(
            interval
                .saturating_sub(started.elapsed())
                .min(deadline - now),
        );
    }
}

fn evaluate(snap: &Snapshot, org: Option<&str>) -> Progress {
    if let Source::LocalScan(reason) = &snap.source {
        return Progress::Unanswerable(format!(
            "{} — the GitHub view comes from the collector, and a local scan cannot see it",
            reason.word()
        ));
    }
    let scope: Vec<&crate::shared::models::RunnerStatus> = snap
        .status
        .runners
        .iter()
        .filter(|r| org.is_none_or(|o| r.org == o))
        .collect();
    if scope.is_empty() {
        return Progress::Unanswerable(match org {
            Some(o) => format!("no runners in org {o} — nothing to wait for"),
            None => "no runners on this host — nothing to wait for".to_string(),
        });
    }

    let total = scope.len();
    let offline = scope
        .iter()
        .filter(|r| r.github_online == Some(false))
        .count();
    let unknown = scope.iter().filter(|r| r.github_online.is_none()).count();
    match (offline, unknown) {
        (0, 0) => Progress::Held,
        // A readable "offline" outranks an unknown: we are early, not blind.
        (pending, _) if pending > 0 => Progress::NotYet { pending, total },
        (_, unknown) => Progress::Blind { unknown, total },
    }
}

fn describe(p: &Progress) -> String {
    match p {
        Progress::Held => "all runners are online to GitHub".to_string(),
        Progress::NotYet { pending, total } => {
            format!("waiting: {pending}/{total} runners still offline to GitHub")
        }
        Progress::Blind { unknown, total } => format!(
            "waiting: {unknown}/{total} runners have no readable GitHub view — a timeout here \
             will report 2 (cannot determine), not 1"
        ),
        Progress::Unanswerable(why) => format!("cannot evaluate: {why}"),
    }
}

fn report(args: &WaitArgs, status: &FleetStatus) -> Result<()> {
    if args.json {
        crate::ops::emit_json(status)?;
    } else {
        crate::ops::emit(&crate::ops::status::human(status))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::ipc::client::EphemeralReason;
    use crate::shared::models::{FleetCounts, Liveness, Mode, RunnerStatus, Verdict};

    fn runner(org: &str, name: &str, github_online: Option<bool>) -> RunnerStatus {
        RunnerStatus {
            name: name.to_string(),
            org: org.to_string(),
            agent_id: 1,
            liveness: Liveness::Idle,
            state_seconds: 0,
            github_online,
            github_busy: None,
            github_offline_seconds: None,
            github_sample_age_s: None,
            divergent: None,
            cpu_percent: None,
            mem_bytes: None,
        }
    }

    fn snap(source: Source, runners: Vec<RunnerStatus>) -> Snapshot {
        Snapshot {
            status: FleetStatus {
                schema_version: 1,
                generated_at: "now".to_string(),
                generated_at_epoch: 0,
                mode: Mode::Persistent,
                verdict: Verdict::Ok,
                fleet: FleetCounts {
                    runners: runners.len() as u32,
                    busy: 0,
                    idle: runners.len() as u32,
                    offline: 0,
                    divergent: 0,
                },
                orgs: Vec::new(),
                runners,
            },
            source,
        }
    }

    #[test]
    fn every_runner_online_holds() {
        let s = snap(
            Source::Collector,
            vec![runner("a", "r1", Some(true)), runner("a", "r2", Some(true))],
        );
        assert_eq!(evaluate(&s, None), Progress::Held);
    }

    #[test]
    fn a_runner_offline_to_github_is_not_yet() {
        let s = snap(
            Source::Collector,
            vec![
                runner("a", "r1", Some(true)),
                runner("a", "r2", Some(false)),
            ],
        );
        assert_eq!(
            evaluate(&s, None),
            Progress::NotYet {
                pending: 1,
                total: 2
            }
        );
    }

    #[test]
    fn the_org_filter_narrows_what_is_waited_on() {
        let s = snap(
            Source::Collector,
            vec![
                runner("a", "r1", Some(true)),
                runner("b", "r2", Some(false)),
            ],
        );
        assert_eq!(evaluate(&s, Some("a")), Progress::Held);
        assert_eq!(
            evaluate(&s, Some("b")),
            Progress::NotYet {
                pending: 1,
                total: 1
            }
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_is_unanswerable_not_satisfied() {
        let s = snap(Source::Collector, vec![runner("a", "r1", Some(true))]);
        match evaluate(&s, Some("typo")) {
            Progress::Unanswerable(why) => assert!(why.contains("typo"), "{why}"),
            other => panic!("expected unanswerable, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_github_view_is_blind_not_offline() {
        let s = snap(
            Source::Collector,
            vec![runner("a", "r1", Some(true)), runner("a", "r2", None)],
        );
        assert_eq!(
            evaluate(&s, None),
            Progress::Blind {
                unknown: 1,
                total: 2
            }
        );
        assert!(describe(&evaluate(&s, None)).contains("cannot determine"));
    }

    #[test]
    fn a_readable_offline_outranks_an_unknown() {
        let s = snap(
            Source::Collector,
            vec![runner("a", "r1", Some(false)), runner("a", "r2", None)],
        );
        assert_eq!(
            evaluate(&s, None),
            Progress::NotYet {
                pending: 1,
                total: 2
            }
        );
    }

    #[test]
    fn without_a_collector_the_predicate_is_unanswerable() {
        let s = snap(
            Source::LocalScan(EphemeralReason::NoCollector),
            vec![runner("a", "r1", None)],
        );
        match evaluate(&s, None) {
            Progress::Unanswerable(why) => assert!(why.contains("no-collector"), "{why}"),
            other => panic!("expected unanswerable, got {other:?}"),
        }
    }
}
