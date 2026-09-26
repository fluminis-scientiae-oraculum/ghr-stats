//! `ghr-stats tail`: polls the collector's timeline and prints each new
//! transition as one JSON line. A poll that hit its limit emits a `gap` line.

use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::cli::TailArgs;
use crate::shared::config::Config;
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::timeline::{
    Bounded, JobTransition, Timeline, TimelineQuery, Transition,
};
use crate::shared::util::now_epoch;

/// Rows fetched per poll, per section.
const POLL_LIMIT: usize = 500;

pub(crate) enum Availability {
    /// Includes the reader closing the pipe (`tail | head`). Ctrl-C never returns
    /// here: the default SIGINT disposition ends the process at 130.
    Followed,
    Unavailable,
}

impl From<Availability> for ExitCode {
    fn from(a: Availability) -> Self {
        ExitCode::from(match a {
            Availability::Followed => 0,
            Availability::Unavailable => 2,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line<'a> {
    Transition(&'a Transition),
    Job(&'a JobTransition),
    /// More rows existed than one poll returned.
    Gap {
        section: &'static str,
        /// The window to re-ask `timeline` for.
        since_epoch: i64,
        until_epoch: i64,
        limit: usize,
    },
}

/// Emitted `(ts, identity)` pairs across the whole rolling window, not a
/// high-water mark: job `ts` comes from the hook's own clock and can arrive late.
/// Pruned to the query horizon.
#[derive(Default)]
struct Cursor {
    seen: HashSet<(i64, String)>,
}

impl Cursor {
    /// True if this event has not been emitted before; records it.
    fn accept(&mut self, ts: i64, key: String) -> bool {
        self.seen.insert((ts, key))
    }

    fn prune(&mut self, before: i64) {
        self.seen.retain(|(ts, _)| *ts >= before);
    }
}

pub fn run(args: &TailArgs, cfg: &Config) -> Result<Availability> {
    let secs = cfg.intervals.local_secs.max(1);
    let interval = Duration::from_secs(secs);
    // Edges are derived with `LAG` inside the query window, so the window must be
    // several ticks deep to hold each edge's predecessor sample. The cursor stops repeats.
    let lookback = (secs * 4).max(60) as i64;
    let mut since = now_epoch() - lookback.max(i64::from(args.backfill));
    let mut transitions = Cursor::default();
    let mut jobs = Cursor::default();
    let mut out = std::io::stdout();

    loop {
        let started = Instant::now();
        let query = TimelineQuery {
            since_ts: since,
            limit: POLL_LIMIT,
            org: args.org.clone(),
            runner: args.runner.clone(),
            samples: false,
        };
        let timeline = match fetch(&query) {
            Some(t) => t,
            None => {
                eprintln!(
                    "cannot tail: no usable collector — the transition record lives there, and \
                     a local scan can only see the present"
                );
                return Ok(Availability::Unavailable);
            }
        };

        match emit(&mut out, &timeline, &mut transitions, &mut jobs) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                return Ok(Availability::Followed);
            }
            Err(e) => return Err(e.into()),
        }
        since = now_epoch() - lookback;
        transitions.prune(since);
        jobs.prune(since);

        std::thread::sleep(interval.saturating_sub(started.elapsed()));
    }
}

/// Connects per poll: holds none of the collector's few connection slots between
/// polls, and survives a collector restart.
fn fetch(query: &TimelineQuery) -> Option<Timeline> {
    let mut client = Client::connect_any().ok()?;
    match client.request(&Request::Query(Query::Timeline(query.clone()))) {
        Ok(Response::Timeline(t)) => Some(*t),
        _ => None,
    }
}

fn emit(
    out: &mut impl Write,
    t: &Timeline,
    transitions: &mut Cursor,
    jobs: &mut Cursor,
) -> std::io::Result<()> {
    // Gap first: a consumer must not act on a batch it believes is complete.
    gap(out, "transitions", &t.transitions, t)?;
    gap(out, "jobs", &t.jobs, t)?;

    for tr in &t.transitions.items {
        if transitions.accept(tr.ts, transition_key(tr)) {
            line(out, &Line::Transition(tr))?;
        }
    }
    for j in &t.jobs.items {
        if jobs.accept(j.ts, job_key(j)) {
            line(out, &Line::Job(j))?;
        }
    }
    Ok(())
}

fn gap<T>(
    out: &mut impl Write,
    section: &'static str,
    b: &Bounded<T>,
    t: &Timeline,
) -> std::io::Result<()> {
    if b.limited {
        line(
            out,
            &Line::Gap {
                section,
                since_epoch: t.window.since_epoch,
                until_epoch: t.window.until_epoch,
                limit: POLL_LIMIT,
            },
        )?;
    }
    Ok(())
}

fn line(out: &mut impl Write, l: &Line) -> std::io::Result<()> {
    let json = serde_json::to_string(l).map_err(std::io::Error::other)?;
    writeln!(out, "{json}")?;
    out.flush()
}

/// Identity within one tick. Includes the org (agent ids and names repeat across
/// orgs) and `to`, so a flap across one tick is two events.
fn transition_key(t: &Transition) -> String {
    use crate::shared::models::timeline::{Edge, ReconcileEdge};
    match &t.edge {
        Edge::Liveness { runner, to, .. } => {
            format!("l|{}|{runner}|{}", t.org, to.as_str())
        }
        Edge::GithubOnline { runner, online } => format!("g|{}|{runner}|{online}", t.org),
        Edge::Reconcile(ReconcileEdge::Recovered) => format!("r|{}|ok", t.org),
        Edge::Reconcile(ReconcileEdge::Failed { error_kind, .. }) => {
            format!("r|{}|fail|{}", t.org, error_kind.as_deref().unwrap_or(""))
        }
    }
}

/// The end distinguishes a start from a completion in the same second.
fn job_key(j: &JobTransition) -> String {
    use crate::shared::models::timeline::JobEdge;
    let end = match j.edge {
        JobEdge::Started => "s",
        JobEdge::Completed { .. } => "c",
    };
    format!("{end}|{}|{}|{}|{}", j.org, j.runner, j.repo, j.job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::Liveness;
    use crate::shared::models::timeline::{Edge, Window};

    fn tr(ts: i64, runner: &str, to: Liveness) -> Transition {
        Transition {
            ts,
            at: String::new(),
            org: "acme".to_string(),
            edge: Edge::Liveness {
                runner: runner.to_string(),
                from: Liveness::Idle,
                to,
            },
        }
    }

    fn timeline(transitions: Vec<Transition>, limited: bool) -> Timeline {
        Timeline {
            schema_version: 1,
            generated_at: String::new(),
            generated_at_epoch: 0,
            window: Window {
                since: String::new(),
                since_epoch: 100,
                until_epoch: 200,
                truncated_at: None,
            },
            transitions: Bounded {
                items: transitions,
                limited,
            },
            jobs: Bounded {
                items: Vec::new(),
                limited: false,
            },
            samples: None,
        }
    }

    fn run_emit(t: &Timeline, c: &mut Cursor) -> String {
        let mut buf = Vec::new();
        let mut jobs = Cursor::default();
        emit(&mut buf, t, c, &mut jobs).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn an_event_already_emitted_is_not_emitted_again() {
        let mut c = Cursor::default();
        let t = timeline(vec![tr(100, "r1", Liveness::Busy)], false);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 1);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 0);
    }

    #[test]
    fn co_timed_events_all_survive_the_cursor() {
        let mut c = Cursor::default();
        let first = timeline(vec![tr(100, "r1", Liveness::Busy)], false);
        assert_eq!(run_emit(&first, &mut c).lines().count(), 1);

        let second = timeline(
            vec![
                tr(100, "r1", Liveness::Busy),
                tr(100, "r2", Liveness::Busy),
                tr(100, "r3", Liveness::Busy),
            ],
            false,
        );
        assert_eq!(run_emit(&second, &mut c).lines().count(), 2);
    }

    #[test]
    fn a_flap_inside_one_tick_is_two_events() {
        let mut c = Cursor::default();
        let t = timeline(
            vec![tr(100, "r1", Liveness::Busy), tr(100, "r1", Liveness::Idle)],
            false,
        );
        assert_eq!(run_emit(&t, &mut c).lines().count(), 2);
    }

    #[test]
    fn the_cursor_forgets_only_what_the_query_can_no_longer_return() {
        let mut c = Cursor::default();
        run_emit(
            &timeline(vec![tr(100, "r1", Liveness::Busy)], false),
            &mut c,
        );
        run_emit(
            &timeline(vec![tr(200, "r1", Liveness::Idle)], false),
            &mut c,
        );
        assert_eq!(c.seen.len(), 2);
        c.prune(150);
        assert_eq!(c.seen.len(), 1);
    }

    #[test]
    fn an_event_older_than_one_already_emitted_still_prints() {
        let mut c = Cursor::default();
        run_emit(
            &timeline(vec![tr(200, "r1", Liveness::Busy)], false),
            &mut c,
        );
        let late = timeline(vec![tr(150, "r2", Liveness::Busy)], false);
        assert_eq!(run_emit(&late, &mut c).lines().count(), 1);
    }

    #[test]
    fn pruning_at_the_query_horizon_does_not_resurrect_an_event() {
        let mut c = Cursor::default();
        let t = timeline(vec![tr(200, "r1", Liveness::Busy)], false);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 1);
        c.prune(200);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 0);
    }

    #[test]
    fn falling_behind_emits_a_gap_line_first() {
        let mut c = Cursor::default();
        let out = run_emit(&timeline(vec![tr(100, "r1", Liveness::Busy)], true), &mut c);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""type":"gap""#), "{out}");
        assert!(lines[0].contains(r#""section":"transitions""#), "{out}");
        assert!(lines[1].contains(r#""type":"transition""#), "{out}");
    }

    #[test]
    fn a_complete_poll_emits_no_gap() {
        let mut c = Cursor::default();
        let out = run_emit(
            &timeline(vec![tr(100, "r1", Liveness::Busy)], false),
            &mut c,
        );
        assert!(!out.contains("gap"), "{out}");
    }

    #[test]
    fn the_same_runner_name_in_two_orgs_is_two_identities() {
        let a = tr(100, "r1", Liveness::Busy);
        let mut b = tr(100, "r1", Liveness::Busy);
        b.org = "other".to_string();
        assert_ne!(transition_key(&a), transition_key(&b));
    }
}
