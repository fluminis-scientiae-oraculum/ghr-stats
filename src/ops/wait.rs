//! `ghr-stats wait` — block until the fleet reaches a state, then exit.
//!
//! This verb exists because it was written by hand three times during the
//! 2026-07-25 investigation, badly each time: a `while ! ghr-stats status …; do
//! sleep 30; done` loop that polls at an arbitrary cadence, cannot tell "not yet"
//! from "cannot see", and reports success the moment its filter matches nothing.
//! Shipping it once means the failure modes are handled once.
//!
//! Three properties are load-bearing.
//!
//! **A timeout while blind is not a timeout.** If the GitHub view was
//! unavailable for the whole wait, the honest answer is "cannot determine" (2),
//! not "it did not happen" (1). A caller branching on the exit code must never
//! read our blindness as the fleet's answer, so the two are different codes.
//!
//! **A predicate that matches nothing is not a predicate that holds.** `--org
//! typo` selects zero runners, and "every runner in this empty set is online" is
//! vacuously true. Exiting 0 on a typo is the worst possible failure for a verb
//! whose entire output is its exit code.
//!
//! **It polls at the rate the data changes**, one `intervals.local_secs` — the
//! same reasoning that decided against a subscribe path on the wire. Asking
//! faster re-reads a sample that has not moved.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::cli::WaitArgs;
use crate::ops::poll::remaining;
use crate::ops::status::{Snapshot, Source};
use crate::shared::config::Config;
use crate::shared::models::FleetStatus;

/// How the wait ended.
///
/// `wait` retrieves rather than judges — like `timeline`, and unlike `status`,
/// `explain` and `doctor` — so it does not return a [`Verdict`]. A fleet that is
/// degraded for an unrelated reason is not this verb's business; the only
/// question is whether the thing asked for happened. The codes still line up
/// with the shared table, so 2 means "cannot determine" whichever verb returned
/// it.
///
/// [`Verdict`]: crate::shared::models::Verdict
pub(crate) enum Outcome {
    /// The predicate held.
    Held,
    /// The deadline passed while we could still see — it genuinely had not
    /// happened yet.
    TimedOut,
    /// We could not evaluate the predicate: no collector, nothing matched the
    /// filter, or the GitHub view never became readable. NEVER conflated with
    /// [`Outcome::TimedOut`].
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

/// One evaluation of the predicate against one snapshot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Every runner in scope satisfies it.
    Held,
    /// Not yet, and we could see well enough to say so — `pending` of `total`
    /// runners are known to be offline to GitHub.
    NotYet { pending: usize, total: usize },
    /// Not yet, and we could not see: some runner in scope has no readable
    /// GitHub view, so "still offline" and "unknown" are indistinguishable.
    Blind { unknown: usize, total: usize },
    /// The question cannot be answered from this snapshot at all.
    Unanswerable(String),
}

/// Run the verb.
pub fn run(args: &WaitArgs, cfg: &Config) -> Result<Outcome> {
    let interval = Duration::from_secs(cfg.intervals.local_secs.max(1));
    let deadline = Instant::now() + Duration::from_secs(args.timeout);

    // What the previous poll reported, so progress is printed on CHANGE rather
    // than once per tick. A 600 s wait at a 5 s cadence is 120 identical lines
    // otherwise, which is how a human learns to stop reading them.
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
            // Fatal only on the FIRST poll. There is nothing to wait for if the
            // collector is absent or the filter matches nothing — telling the
            // caller now beats telling them in ten minutes. Later, the same
            // condition is a collector that restarted mid-wait, which the next
            // poll may well resolve.
            Progress::Unanswerable(why) if first => {
                eprintln!("cannot wait: {why}");
                report(args, &snap.status)?;
                return Ok(Outcome::Undetermined);
            }
            Progress::Unanswerable(_) | Progress::Blind { .. } | Progress::NotYet { .. } => {}
        }
        // Whether THIS poll could see the GitHub view. Scoped to the iteration
        // because it is read in the same one: a wait that reaches its deadline
        // blind reports 2, not 1 — see the module docs.
        let blind = matches!(progress, Progress::Blind { .. } | Progress::Unanswerable(_));
        if last.as_ref() != Some(&progress) {
            eprintln!("{}", describe(&progress));
            last = Some(progress);
        }
        first = false;

        // Checked AFTER evaluating, so `--timeout 0` is a single evaluation
        // rather than a special case in the loop.
        let now = Instant::now();
        if now >= deadline {
            report(args, &snap.status)?;
            return Ok(if blind {
                Outcome::Undetermined
            } else {
                Outcome::TimedOut
            });
        }
        // Never sleep past the deadline: the remaining wait is the shorter of
        // the poll cadence and the time actually left.
        std::thread::sleep(remaining(started.elapsed(), interval).min(deadline - now));
    }
}

/// Evaluate `--github-online` against a snapshot. Pure — every branch below is
/// tested without a socket or a fleet.
fn evaluate(snap: &Snapshot, org: Option<&str>) -> Progress {
    // Ephemeral means a live local scan: it can see processes, never GitHub. The
    // predicate is about GitHub, so this is unanswerable rather than false —
    // the distinction between "they are not online" and "we cannot ask".
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
        // A readable "no" outranks an unreadable "maybe": if anything is
        // definitely offline we are not blind, we are early.
        (pending, _) if pending > 0 => Progress::NotYet { pending, total },
        (_, unknown) => Progress::Blind { unknown, total },
    }
}

/// One line of progress, for stderr. Never stdout: stdout carries the final
/// snapshot, which something may be parsing.
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

/// Print the snapshot the wait ended on. Emitted on every outcome, not only on
/// failure: a caller that waited ten minutes for success still wants the state
/// it succeeded in, and one that timed out needs it to know why.
fn report(args: &WaitArgs, status: &FleetStatus) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        print!("{}", crate::ops::status::human(status));
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

    /// The filter narrows the question. A wait for one org must not be held up
    /// by another org's outage — that is the whole reason `--org` exists.
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

    /// A filter that matches nothing must NEVER report success. "Every runner in
    /// the empty set is online" is vacuously true and would exit 0 on a typo —
    /// the worst failure available to a verb whose output is its exit code.
    #[test]
    fn a_filter_that_matches_nothing_is_unanswerable_not_satisfied() {
        let s = snap(Source::Collector, vec![runner("a", "r1", Some(true))]);
        match evaluate(&s, Some("typo")) {
            Progress::Unanswerable(why) => assert!(why.contains("typo"), "{why}"),
            other => panic!("expected unanswerable, got {other:?}"),
        }
    }

    /// An unreadable GitHub view is not the same as a runner being offline, and
    /// the difference decides the exit code: blind at the deadline is 2.
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

    /// A definite "offline" outranks an unknown: if anything is provably not
    /// there yet, we are early rather than blind, and a timeout is an honest 1.
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

    /// Without a collector the predicate is about something we cannot observe.
    /// Waiting ten minutes to discover that is the failure this arm prevents.
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

    #[test]
    fn the_exit_codes_match_the_shared_table() {
        let code = |o: Outcome| match o {
            Outcome::Held => 0,
            Outcome::TimedOut => 1,
            Outcome::Undetermined => 2,
        };
        assert_eq!(code(Outcome::Held), 0);
        assert_eq!(code(Outcome::TimedOut), 1);
        assert_eq!(code(Outcome::Undetermined), 2);
    }
}
