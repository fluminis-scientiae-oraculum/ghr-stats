//! Metric [`Snapshot`] read from the store. [`exposition`] renders it for metrics backends;
//! [`status`] adjudicates it into a [`FleetStatus`].

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::error::Result;
use crate::shared::models::{self, ApiReconcileState, GhView, Liveness};

mod exposition;
mod status;

struct RunnerMetric {
    agent_id: i64,
    name: String,
    org: String,
    liveness: Liveness,
    cpu_pct: Option<f32>,
    mem_bytes: Option<u64>,
    mem_current_bytes: Option<u64>,
    /// Time in the current liveness state.
    state_seconds: i64,
    /// Whole verdict, not flattened booleans, so "GitHub says offline" stays distinct from
    /// "no current reading".
    gh: GhView,
    /// Continuous time offline to GitHub, from the persisted edge. `None` when online or no
    /// edge yet.
    gh_offline_seconds: Option<i64>,
}

impl RunnerMetric {
    fn divergent(&self) -> Option<bool> {
        models::divergent(self.liveness, self.gh)
    }
}

struct OrgRollup {
    org: String,
    total: u32,
    /// Runners with a fresh GitHub view. A runner we cannot see is not one GitHub reports offline.
    github_known: u32,
    github_online: u32,
}

/// Gathered once per scrape/push.
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
    /// Configured freshness window (seconds), exported so alerts need not hardcode it.
    max_age: u64,
}

impl Snapshot {
    /// `max_age`: seconds a GitHub reconcile row stays current.
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
                let gh = api
                    .get(&(r.org.clone(), r.agent_id))
                    .copied()
                    .unwrap_or(GhView::Unknown);
                // From the persisted edge, so it survives collector restarts and scrape gaps.
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

        let mut by_org: BTreeMap<&str, (u32, u32, u32)> = BTreeMap::new();
        for r in &runners {
            let e = by_org.entry(r.org.as_str()).or_default();
            e.0 += 1;
            // `online()` is `None` for a stale or missing reading: neither known nor online.
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
