//! Metric gathering + rendering. Reads the store on a caller-provided
//! connection, builds a [`Snapshot`], and hands it to one of two renderers.
//! Pure reads + formatting — no DB writes, no I/O of its own.
//!
//! Cut by WHO IS ASKING, AND WHAT THEY GET BACK:
//!
//! - **this file** — the snapshot itself: the row types, and the one read that
//!   fills them from the store.
//! - [`exposition`] — what a METRICS BACKEND gets: the same numbers in two
//!   syntaxes, Prometheus text for the pull endpoint and a flat JSON array for
//!   the push sink. Neither ever judges.
//! - [`status`] — what the WIRE gets: the adjudicated [`FleetStatus`], the only
//!   projection that computes a [`Verdict`].
//!
//! That is the codebase's RETRIEVE-vs-JUDGE line drawn inside one module, and
//! the import lists agree exactly: `FleetCounts`, `OrgStatus`, `RunnerStatus`,
//! `Verdict` and `Mode` appear only in [`status`]; `esc` only in [`exposition`];
//! `reader` only here. History agrees too — `d695138` (the `status` verb) is the
//! only commit to have touched one of these three alone.
//!
//! Two members resist the cut in opposite directions, and each says something.
//! `RunnerMetric::labels` renders a Prometheus label set, so it is exposition
//! syntax rather than a property of the row, and it moves to [`exposition`] with
//! `esc`. [`RunnerMetric::divergent`] stays HERE because BOTH children call it —
//! a derivation two consumers share belongs above the cut, not on either side of
//! it, which is also why the exporter and the TUI cannot disagree about it.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::error::Result;
use crate::shared::models::{self, ApiReconcileState, GhView, Liveness};

mod exposition;
mod status;

/// One runner's metric row.
struct RunnerMetric {
    agent_id: i64,
    name: String,
    org: String,
    liveness: Liveness,
    cpu_pct: Option<f32>,
    mem_bytes: Option<u64>,
    mem_current_bytes: Option<u64>,
    /// Seconds in the current liveness state (`now - since_ts`).
    state_seconds: i64,
    /// GitHub's view, freshness already adjudicated by the reader. Held as the
    /// whole verdict rather than pre-flattened booleans so the exporter can
    /// distinguish "GitHub says offline" from "we have no current reading" —
    /// the distinction the all-green rollup was missing.
    gh: GhView,
    /// Seconds this runner has been continuously offline *to GitHub*, from the
    /// persisted edge. `None` when online, or when there is no edge yet.
    gh_offline_seconds: Option<i64>,
}

impl RunnerMetric {
    /// See [`models::divergent`] — the one place this verdict is derived, shared
    /// with the TUI header so the exporter and the dashboard cannot disagree.
    fn divergent(&self) -> Option<bool> {
        models::divergent(self.liveness, self.gh)
    }
}

/// Per-org rollup. The 62-vs-0/0/0 arithmetic across orgs is what turned "is
/// this abnormal?" into a decidable question during the incident, so it is
/// exported rather than left for a query to reconstruct.
struct OrgRollup {
    org: String,
    total: u32,
    /// Runners for which GitHub's view is FRESH — i.e. an opinion we may hold.
    /// Distinct from `total`, because a runner we cannot currently see is not a
    /// runner GitHub reports as offline.
    github_known: u32,
    github_online: u32,
}

/// A point-in-time metrics snapshot, gathered once per scrape/push.
pub struct Snapshot {
    version: String,
    now: i64,
    last_sample_ts: Option<i64>,
    runners: Vec<RunnerMetric>,
    busy: u32,
    idle: u32,
    offline: u32,
    load1: Option<f64>,
    mem_used: Option<u64>,
    mem_total: Option<u64>,
    jobs_total: i64,
    jobs_running: i64,
    /// Runners that are locally up while GitHub says they cannot take work.
    divergent: u32,
    orgs: Vec<OrgRollup>,
    reconcile: Vec<ApiReconcileState>,
    /// The configured freshness window, exported so a scrape is self-describing
    /// and an alert can reference the operator's value instead of hardcoding it.
    max_age: u64,
}

impl Snapshot {
    /// Read the current fleet state into a snapshot. `max_age` bounds how old a
    /// GitHub reconcile row may be and still count as current — see
    /// [`crate::shared::config::Intervals::api_max_age`], the one place it is
    /// decided.
    pub fn gather(conn: &Connection, now: i64, version: &str, max_age: u64) -> Result<Snapshot> {
        let latest = reader::latest_runners(conn)?;
        let states = reader::runner_states(conn)?;
        let api = reader::latest_api_runners(conn, now, max_age)?;
        let api_edges = reader::api_runner_states(conn)?;
        let reconcile = reader::api_reconcile_states(conn)?;
        let host = reader::latest_host(conn)?;
        let (jobs_total, jobs_running) = reader::job_counts(conn)?;

        let last_sample_ts = latest.iter().map(|r| r.ts).max();
        let (mut busy, mut idle, mut offline) = (0u32, 0u32, 0u32);
        let runners = latest
            .into_iter()
            .map(|r| {
                match r.liveness {
                    Liveness::Busy => busy += 1,
                    Liveness::Idle => idle += 1,
                    Liveness::Offline => offline += 1,
                }
                let state_seconds = states
                    .get(&r.dir)
                    .map(|s| (now - s.since_ts).max(0))
                    .unwrap_or(0);
                // A runner with no reconcile row at all is Unknown — never
                // silently folded into "offline".
                let gh = api
                    .get(&(r.org.clone(), r.agent_id))
                    .copied()
                    .unwrap_or(GhView::Unknown);
                // Offline duration comes from the persisted edge, not from the
                // instantaneous bit: it survives collector restarts and scrape
                // gaps, which is the same reason `runner_state` exists locally.
                let gh_offline_seconds = api_edges
                    .get(&(r.org.clone(), r.agent_id))
                    .filter(|e| !e.online)
                    .map(|e| (now - e.since_ts).max(0));
                RunnerMetric {
                    agent_id: r.agent_id,
                    name: r.name,
                    org: r.org,
                    liveness: r.liveness,
                    cpu_pct: r.cpu_pct,
                    mem_bytes: r.mem_bytes,
                    mem_current_bytes: r.mem_current_bytes,
                    state_seconds,
                    gh,
                    gh_offline_seconds,
                }
            })
            .collect::<Vec<RunnerMetric>>();

        let divergent = runners
            .iter()
            .filter(|r| r.divergent() == Some(true))
            .count() as u32;

        // Per-org totals. Only FRESH readings count as online — a stale row must
        // not inflate an org's health.
        let mut by_org: BTreeMap<&str, (u32, u32, u32)> = BTreeMap::new();
        for r in &runners {
            let e = by_org.entry(r.org.as_str()).or_default();
            e.0 += 1;
            // A reading we HAVE counts toward `known`; only a positive one
            // counts toward `online`. `None` is neither — it is the absence the
            // verdict must not read as a failure.
            if let Some(online) = r.gh.online() {
                e.1 += 1;
                if online {
                    e.2 += 1;
                }
            }
        }
        let orgs = by_org
            .into_iter()
            .map(|(org, (total, github_known, github_online))| OrgRollup {
                org: org.to_string(),
                total,
                github_known,
                github_online,
            })
            .collect();

        Ok(Snapshot {
            version: version.to_string(),
            now,
            last_sample_ts,
            runners,
            busy,
            idle,
            offline,
            load1: host.as_ref().map(|h| h.load1),
            mem_used: host.as_ref().map(|h| h.mem_used),
            mem_total: host.as_ref().map(|h| h.mem_total),
            jobs_total,
            jobs_running,
            divergent,
            orgs,
            reconcile,
            max_age,
        })
    }
}
