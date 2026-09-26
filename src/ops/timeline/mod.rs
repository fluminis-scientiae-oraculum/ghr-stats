//! `ghr-stats timeline`: what changed over a window, as edges. No local-scan
//! fallback: history exists only in the collector's database. The exit code
//! reports availability, never health.

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::TimelineArgs;
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::timeline::TimelineQuery;
use crate::shared::util::now_epoch;

mod render;
pub(crate) mod since;

use render::human;

/// Bounds one call's output, not storage (`db prune` keeps 14 days by default).
const MAX_WINDOW_SECS: u64 = 7 * 86_400;

pub(crate) enum Availability {
    Answered,
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

pub fn run(args: &TimelineArgs) -> Result<Availability> {
    let window = args.since;
    if window.clamped {
        eprintln!("note: --since is capped at {window}");
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
        crate::ops::emit_json(&timeline)?;
    } else {
        crate::ops::emit(&human(&timeline, &window.to_string()))?;
    }
    Ok(Availability::Answered)
}
