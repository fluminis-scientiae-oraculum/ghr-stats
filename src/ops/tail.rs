//! `ghr-stats tail` — the fleet's transitions as they happen, one JSON object
//! per line.
//!
//! **This is a poll, not a subscription, and that was the phase-6 decision
//! rather than an implementation shortcut** (the reasoning lives in
//! `shared::ipc`). The short version: a transition does not exist until a
//! sampler observes it, so a subscriber would receive events on the same
//! `local_secs` grid a poller sees; a held-open stream would occupy one of the
//! collector's few connection slots for its lifetime, which is the lockout
//! `4b3b490` was written to end; and a poller can PROVE it kept up, because
//! `Bounded` reports truncation, whereas a stream that drops under backpressure
//! has to be built to admit it.
//!
//! Two properties follow from that and are load-bearing here.
//!
//! **Falling behind is emitted, never swallowed.** If more transitions occurred
//! than the limit returned, `tail` prints a `gap` object naming the section and
//! the window it could not fully cover. A watcher that silently skips events is
//! worse than no watcher, because its silence is indistinguishable from calm.
//!
//! **The cursor is a timestamp AND the identities already emitted at it.**
//! `since_ts` is inclusive and a fleet routinely flips several runners on one
//! tick, so an exclusive cursor would drop the co-timed edges and an inclusive
//! one would repeat them forever. Carrying the keys seen at the newest tick
//! costs memory proportional to the fleet, not to the window.

use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::cli::TailArgs;
use crate::ops::poll::remaining;
use crate::shared::config::Config;
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::timeline::{
    Bounded, JobTransition, Timeline, TimelineQuery, Transition,
};
use crate::shared::util::now_epoch;

/// Rows fetched per poll, per section.
///
/// Generous against a five-second window — this fleet's busiest observed minute
/// is far under it — so `limited` firing means something genuinely unusual
/// happened rather than that the default was too tight.
const POLL_LIMIT: usize = 500;

/// Whether the stream could be followed at all.
///
/// `tail` retrieves rather than judges, so like `timeline` and `wait` it does
/// not return a [`Verdict`]: an eventful window and a quiet one are equally
/// successful tails. Only losing the collector ends it unsuccessfully.
///
/// [`Verdict`]: crate::shared::models::Verdict
pub(crate) enum Availability {
    /// The reader closed the pipe — `ghr-stats tail | head -5` is an ordinary
    /// invocation and must end at 0, not at the usage code an unhandled
    /// `BrokenPipe` would produce. (A Ctrl-C never reaches here: the default
    /// SIGINT disposition ends the process at 130, the shell's own convention
    /// for a stream stopped by its operator.)
    Followed,
    /// There is no collector, so there is nothing to follow.
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

/// One emitted line. Externally tagged so a consumer can branch on `type`
/// without inspecting the shape, and so `gap` is impossible to mistake for an
/// event.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line<'a> {
    Transition(&'a Transition),
    Job(&'a JobTransition),
    /// We fell behind: more rows existed than one poll returned.
    Gap {
        section: &'static str,
        /// The window that could not be fully covered, so the caller knows
        /// exactly what to re-ask `timeline` for.
        since_epoch: i64,
        until_epoch: i64,
        limit: usize,
    },
}

/// What has already been emitted, so an overlapping poll does not repeat it.
///
/// Keyed by `(ts, identity)` across the whole rolling window rather than by a
/// high-water timestamp. A high-water mark is wrong twice over. It drops a
/// late arrival — `job_event` timestamps come from the hook's own clock and are
/// ingested by a tailer that can lag, so a job may legitimately surface with a
/// `ts` behind one already emitted. And it says nothing about the query window,
/// which is the part that actually has to overlap: edges are derived with `LAG`
/// over the rows *inside* the window, so an edge exists only when its
/// PREDECESSOR SAMPLE is also inside. Re-asking from the last event emitted
/// would leave that predecessor outside and silently suppress the edge — and
/// because job events are frequent and liveness edges are rare, a busy fleet
/// would suppress exactly the transitions worth watching.
///
/// Pruned to the same horizon as the query, so memory tracks the window rather
/// than the uptime, and nothing is forgotten that could still be returned.
#[derive(Default)]
struct Cursor {
    seen: HashSet<(i64, String)>,
}

impl Cursor {
    /// True if this event has not been emitted before; records it.
    fn accept(&mut self, ts: i64, key: String) -> bool {
        self.seen.insert((ts, key))
    }

    /// Forget events the query can no longer return.
    fn prune(&mut self, before: i64) {
        self.seen.retain(|(ts, _)| *ts >= before);
    }
}

/// Run the verb. Returns only when the collector is unreachable — otherwise the
/// caller ends it (Ctrl-C), which is why there is no disconnect handling here.
pub fn run(args: &TailArgs, cfg: &Config) -> Result<Availability> {
    let secs = cfg.intervals.local_secs.max(1);
    let interval = Duration::from_secs(secs);
    // Every poll asks for a ROLLING window, never for "everything since the last
    // event". The window has to be wide enough to contain the predecessor sample
    // of any edge inside it — see `Cursor` — so it is several sampling intervals
    // deep, with a floor for hosts sampling fast enough that four ticks is only
    // a few seconds. The cursor, not the window, is what stops repeats.
    let lookback = (secs * 4).max(60) as i64;
    // The first poll may reach further back, but only if asked: `tail` answers
    // "what is happening", and `timeline --since` already answers "what
    // happened", so backfill is a flag rather than a surprise flood.
    let mut since = now_epoch() - lookback.max(args.since_secs() as i64);
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
            // Our reader went away — `| head`, a closed pager, a killed consumer.
            // That is the pipeline working, not a failure of ours.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                return Ok(Availability::Followed);
            }
            Err(e) => return Err(e.into()),
        }
        // The window rolls with the clock rather than chasing the last event.
        // Forget only what the next query can no longer return, so the two stay
        // exactly in step: nothing is remembered needlessly, and nothing that
        // could still arrive is forgotten.
        since = now_epoch() - lookback;
        transitions.prune(since);
        jobs.prune(since);

        std::thread::sleep(remaining(started.elapsed(), interval));
    }
}

/// One poll. Connects per iteration rather than holding a client: a connection
/// held across polls is the resource a subscribe path would have monopolised,
/// and reconnecting is also what lets a `tail` survive a collector restart.
fn fetch(query: &TimelineQuery) -> Option<Timeline> {
    let mut client = Client::connect_any().ok()?;
    match client.request(&Request::Query(Query::Timeline(query.clone()))) {
        Ok(Response::Timeline(t)) => Some(*t),
        _ => None,
    }
}

/// Print everything in this poll that has not been printed before, oldest
/// first, flushing per line so a consumer reading our stdout sees each event as
/// it lands rather than when the pipe buffer happens to fill.
fn emit(
    out: &mut impl Write,
    t: &Timeline,
    transitions: &mut Cursor,
    jobs: &mut Cursor,
) -> std::io::Result<()> {
    // The gap goes FIRST, before the events it qualifies: a consumer that reads
    // the events and then learns some were missing has already acted on a set it
    // believed was complete.
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
    // Serialisation of a borrowed, closed enum cannot realistically fail, but it
    // is folded into the io error rather than unwrapped: a panic in a long-lived
    // watcher is the one failure mode with no diagnostic left behind.
    let json = serde_json::to_string(l).map_err(std::io::Error::other)?;
    writeln!(out, "{json}")?;
    // Per line, not per poll: a consumer piping this into `jq` or an agent loop
    // must see an event when it happens, not when the pipe buffer fills.
    out.flush()
}

/// Identity of a transition within one tick.
///
/// Must distinguish everything that can legitimately co-occur: two runners in
/// different orgs share an `agent_id` on this fleet, and one runner can produce
/// a liveness edge and a GitHub edge at the same instant. The `to` value is part
/// of the key so a flap back and forth across one tick is two events, not one
/// swallowed by de-duplication.
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

/// Identity of a job edge within one tick. `(run, job, runner)` is the table's
/// own key; the end distinguishes a start from a completion recorded at the same
/// second, which a fast job does produce.
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

    /// The overlap is deliberate — we re-ask from the newest tick we emitted —
    /// so the cursor, not the query, is what stops an event printing twice.
    #[test]
    fn an_event_already_emitted_is_not_emitted_again() {
        let mut c = Cursor::default();
        let t = timeline(vec![tr(100, "r1", Liveness::Busy)], false);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 1);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 0);
    }

    /// The case an exclusive `> ts` cursor gets wrong: several runners flipping
    /// on one sampler tick is the NORMAL shape of an incident, and dropping all
    /// but the first would hide exactly the correlated failure worth watching.
    #[test]
    fn co_timed_events_all_survive_the_cursor() {
        let mut c = Cursor::default();
        let first = timeline(vec![tr(100, "r1", Liveness::Busy)], false);
        assert_eq!(run_emit(&first, &mut c).lines().count(), 1);

        // The next poll overlaps and returns r1 again plus two more at the SAME
        // timestamp. Only the two new ones may print.
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

    /// A runner that flips away and back within one tick is two events. Keying
    /// on identity alone would swallow the second and leave the watcher
    /// believing the first never reversed.
    #[test]
    fn a_flap_inside_one_tick_is_two_events() {
        let mut c = Cursor::default();
        let t = timeline(
            vec![tr(100, "r1", Liveness::Busy), tr(100, "r1", Liveness::Idle)],
            false,
        );
        assert_eq!(run_emit(&t, &mut c).lines().count(), 2);
    }

    /// Memory tracks the WINDOW, not the uptime: pruning drops exactly what the
    /// next query can no longer return, and nothing else.
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

    /// A late arrival must still print. `job_event` timestamps come from the
    /// hook's own clock via a tailer that can lag, so an event whose `ts` sits
    /// behind one already emitted is normal — and a high-water cursor drops it
    /// silently, which is the reason this one keys on the whole window.
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

    /// Pruning must not resurrect an event the next query still returns. The
    /// horizon is shared with the query window precisely so the two cannot
    /// disagree; an event exactly AT the horizon is still returnable and so must
    /// still be remembered.
    #[test]
    fn pruning_at_the_query_horizon_does_not_resurrect_an_event() {
        let mut c = Cursor::default();
        let t = timeline(vec![tr(200, "r1", Liveness::Busy)], false);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 1);
        c.prune(200);
        assert_eq!(run_emit(&t, &mut c).lines().count(), 0);
    }

    /// Falling behind must be visible, and visible BEFORE the events it
    /// qualifies — a consumer that acts on the batch first has already acted on
    /// a set it believed was complete.
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

    /// Two orgs share `agent_id` 22 on this fleet, so the org must be part of
    /// the key or one org's edge would suppress the other's.
    #[test]
    fn the_same_runner_name_in_two_orgs_is_two_identities() {
        let a = tr(100, "r1", Liveness::Busy);
        let mut b = tr(100, "r1", Liveness::Busy);
        b.org = "other".to_string();
        assert_ne!(transition_key(&a), transition_key(&b));
    }
}
