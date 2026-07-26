//! `ghr-stats timeline` — what changed over a window.
//!
//! `status` answers *is the fleet healthy now*; `explain` answers *why isn't
//! it*. Neither can answer *when did this start, and what moved first* — and
//! that ordering is what turns a set of symptoms into a cause. The 2026-07-25
//! investigation reached for it by hand: two SQL queries against a private
//! schema, because the ordering existed in the database and nowhere in the
//! interface.
//!
//! Two properties are load-bearing.
//!
//! **Edges, not samples.** A six-hour window on this fleet is tens of thousands
//! of rows that mostly say "still idle". The same window as transitions is a
//! handful of lines, and the three streams — local liveness, GitHub's view, the
//! per-org reconcile outcome — are kept separate precisely so their
//! disagreement stays visible.
//!
//! **Bounded by construction.** Every collection carries whether it was cut, the
//! window is capped before it leaves this process and clamped again by the
//! collector, and a window reaching past what `db prune` left says so. An agent
//! that cannot trust the bound has to fetch everything to find out, which is the
//! situation the verb exists to end.
//!
//! Unlike `status` and `explain` there is no local-scan fallback: history exists
//! only in the collector's database. A live scan can say what is true now; it
//! cannot say what was true an hour ago, and inventing a one-point "timeline"
//! from the present would answer a different question than the one asked.
//!
//! **Availability, not health.** This verb RETRIEVES; it does not judge. Its
//! exit code says whether the question could be answered, never whether the
//! fleet is well — so `timeline && echo healthy` is a bug, not an idiom.
//!
//! Cut into THE QUESTION IN and THE ANSWER OUT. [`since`] turns `--since 6h`
//! into a bounded window; [`render`] turns a `Timeline` into text. This file
//! keeps `run`, which is the middle, and [`Availability`], which is what `run`
//! returns. Both children are pure — one parses, one formats, neither touches
//! the socket — which is why nearly every test in this module lives with one of
//! them rather than here.

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::TimelineArgs;
use crate::shared::config::Config;
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::timeline::TimelineQuery;
use crate::shared::util::now_epoch;

mod render;
mod since;

use render::human;
use since::parse_since;

/// The furthest back a window may reach.
///
/// Not a storage limit — `db prune` keeps 14 days by default — but a bound on
/// what one call may be asked to produce. Seven days is past every "what
/// happened overnight / over the weekend" question while staying an order of
/// magnitude short of a window whose transitions would swamp any caller.
const MAX_WINDOW_SECS: u64 = 7 * 86_400;

/// Whether the question could be answered at all.
///
/// `timeline` makes no health call, so its exit code must not pretend to: a
/// window with no transitions in it is a perfectly healthy answer, and mapping
/// that to [`crate::shared::models::Verdict::Ok`] would let `timeline && echo
/// healthy` claim something this verb never assessed. What it *can* report is
/// availability — and the two codes it uses keep the meanings the `status` table
/// already gives them, so one vocabulary spans the verbs.
pub(crate) enum Availability {
    /// The window was answered.
    Answered,
    /// No collector, so there is no history to answer from — "cannot
    /// determine", the same 2 `status` returns when it cannot say anything.
    Unavailable,
}

impl From<Availability> for ExitCode {
    fn from(a: Availability) -> Self {
        ExitCode::from(match a {
            Availability::Answered => 0,
            Availability::Unavailable => 2,
        })
    }
}

/// Run the verb.
pub fn run(args: &TimelineArgs, _cfg: &Config) -> Result<Availability> {
    let window = parse_since(&args.since)?;
    if window.clamped {
        // The effective window is in the payload either way, but an operator who
        // asked for 30d should not have to read a field to discover they got 7.
        eprintln!(
            "note: --since {} exceeds the {}d maximum window; using 7d",
            args.since,
            MAX_WINDOW_SECS / 86_400
        );
    }

    let mut client = match Client::connect_any() {
        Ok(c) => c,
        Err(reason) => {
            eprintln!(
                "cannot read history: {} — timeline needs the collector, which is the only \
                 thing that keeps a record; a local scan can only see the present.",
                reason.word()
            );
            return Ok(Availability::Unavailable);
        }
    };

    let query = TimelineQuery {
        since_ts: now_epoch() - window.secs as i64,
        limit: args.limit,
        org: args.org.clone(),
        runner: args.runner.clone(),
        samples: args.samples,
    };
    let timeline = match client.request(&Request::Query(Query::Timeline(query)))? {
        Response::Timeline(t) => *t,
        // The collector is up and speaks our wire version, so this is its own
        // fault (a database error, almost always) — the same distinction
        // `explain` draws between "absent" and "broken".
        Response::Error(e) => {
            eprintln!("cannot read history: the collector answered with an error: {e}");
            return Ok(Availability::Unavailable);
        }
        other => {
            eprintln!("cannot read history: unexpected reply from the collector: {other:?}");
            return Ok(Availability::Unavailable);
        }
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&timeline)?);
    } else {
        print!("{}", human(&timeline, &args.since));
    }
    Ok(Availability::Answered)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exit code is availability, never health: an answered window exits 0
    /// even when it is full of failures, and a missing collector exits 2 — the
    /// same "cannot determine" `status` uses.
    #[test]
    fn exit_codes_carry_availability_not_health() {
        assert_eq!(
            format!("{:?}", ExitCode::from(Availability::Answered)),
            format!("{:?}", ExitCode::from(0))
        );
        assert_eq!(
            format!("{:?}", ExitCode::from(Availability::Unavailable)),
            format!("{:?}", ExitCode::from(2))
        );
    }
}
