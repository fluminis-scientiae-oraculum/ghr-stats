//! Server-side reads (the IPC server + the metrics exporter). The collector is
//! the only writer; readers open their own connection and rely on WAL for
//! contention-free concurrent reads. The TUI no longer opens the DB — in
//! Persistent mode it fetches these same shapes over the IPC socket, so the
//! query structs below double as the IPC wire payloads (hence the serde derives).
//!
//! Cut by WHICH PRODUCER WROTE THE ROWS, because that is what a schema change
//! follows. `serve` runs those producers as separate threads on separate
//! periods, and each one owns its own tables:
//!
//! - **this file** — what the collector SAMPLED off this machine: the runner and
//!   host ticks.
//! - [`github`] — what GITHUB said, from the reconcile thread's writes.
//! - [`jobs`] — what the RUNNERS' HOOKS reported: the ingested event log, and
//!   the tailer's own place in the streams it read them from.
//! - [`timeline`] — the one cluster with a shape rather than a source of its own:
//!   every function elsewhere answers "what is true now", while those derive what
//!   CHANGED between two samples.
//!
//! `V6` (the `api_*` tables) landed entirely in [`github`]; the job-conclusion
//! reconcile landed entirely in [`jobs`]. The model types agree with the split —
//! `GhView`/`ApiState` appear only in one, `JobRow`/`PendingConclusion` only in
//! another, `OptionalExtension` only where a query may legitimately match nothing.
//!
//! [`busy_series`] stays here despite joining GitHub's rows: it is DRIVEN by the
//! local tick and enriches it, which is exactly why it is the series that can
//! show the two disagreeing.
//!
//! Every reader is re-exported flat, so callers keep saying `reader::recent_jobs`
//! and have no reason to learn which producer's file it moved to.

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::models::{
    BusyPoint, GhCount, HistPoint, HostPoint, Liveness, RunnerSample, RunnerState,
};

/// The `timeline` query's readers — the edge derivations and the window
/// assembly. Split out because they are the one cluster here with a shape of
/// their own: every other function in this module answers "what is true now",
/// while these derive what CHANGED between two samples.
pub mod timeline;

// Re-exported so callers keep saying `reader::timeline(..)`. The module and the
// function share a name deliberately — the module IS that query's readers, and
// a caller has no reason to learn that the entry point moved.
pub use timeline::timeline;

mod github;
mod jobs;

pub use github::{api_reconcile_states, api_runner_states, latest_api_runners};
pub use jobs::{ingest_offsets, job_counts, jobs_awaiting_conclusion, latest_job, recent_jobs};

/// The most recent `limit` samples for a runner, returned oldest → newest so
/// they can be fed straight into a left-to-right sparkline.
pub fn runner_history(conn: &Connection, dir: &str, limit: usize) -> Result<Vec<HistPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, cpu_pct, mem_bytes FROM runner_sample \
         WHERE dir = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![dir, limit as i64], |r| {
        Ok(HistPoint {
            ts: r.get(0)?,
            cpu_pct: r.get::<_, Option<f64>>(1)?.map(|v| v as f32),
            mem_bytes: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        })
    })?;
    let mut out: Vec<HistPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

/// Recent host samples, oldest → newest.
pub fn host_series(conn: &Connection, limit: usize) -> Result<Vec<HostPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, load1, mem_used, mem_total, tmp_bytes, work_bytes, root_free FROM host_sample \
         ORDER BY ts DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(HostPoint {
            ts: r.get(0)?,
            load1: r.get(1)?,
            mem_used: r.get::<_, i64>(2)? as u64,
            mem_total: r.get::<_, i64>(3)? as u64,
            tmp_bytes: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            work_bytes: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            root_free: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        })
    })?;
    let mut out: Vec<HostPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

/// Fleet occupancy per tick (busy and online counts), oldest → newest.
///
/// The GitHub count is LEFT-joined per runner from the newest reconcile reading
/// AT OR BEFORE that tick, bounded by `max_age` — the same shape
/// [`timeline_samples`] uses, for the same reason. The two producers are
/// independent threads on different periods (local ticks default to 5s, API
/// ticks to 60s) that each stamp their own clock, so joining on exact `ts`
/// equality only matched when both happened to fire inside the same second:
/// roughly one point per twelve, and the density tracked scheduler drift rather
/// than whether GitHub data existed. Carrying the last reading forward gives a
/// continuous series; `max_age` is what keeps it honest, so a dead reconcile
/// thread decays into a gap instead of a confident flat line.
///
/// Joining per `(org, agent_id)` also narrows the count to OUR runners: the old
/// query summed every API row at the matching tick, which on an org whose
/// runners are spread across hosts counted machines this host cannot see.
///
/// Deriving occupancy from local liveness alone is what let the Trends chart
/// draw a flat healthy line straight through a four-hour outage.
pub fn busy_series(conn: &Connection, limit: usize, max_age: u64) -> Result<Vec<BusyPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.ts, \
                SUM(r.liveness = 'busy') AS busy, \
                SUM(r.liveness <> 'offline') AS online, \
                SUM(a.online) AS gh_online, \
                SUM(a.ts IS NOT NULL) AS gh_known \
         FROM runner_sample r \
         LEFT JOIN api_runner_sample a \
                ON a.org = r.org AND a.agent_id = r.agent_id \
               AND a.ts = (SELECT max(x.ts) FROM api_runner_sample x \
                            WHERE x.org = r.org AND x.agent_id = r.agent_id \
                              AND x.ts <= r.ts AND x.ts >= r.ts - ?2) \
         GROUP BY r.ts ORDER BY r.ts DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64, max_age as i64], |r| {
        Ok(BusyPoint {
            ts: r.get(0)?,
            busy: r.get::<_, i64>(1)? as u32,
            online: r.get::<_, i64>(2)? as u32,
            // `gh_online` is NULL exactly when no row joined, which is the same
            // condition as `gh_known == 0`; `GhCount::new` owns that decision.
            github: GhCount::new(
                r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32,
                r.get::<_, i64>(4)? as u32,
            ),
        })
    })?;
    let mut out: Vec<BusyPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

/// Every runner's most recent sample (the latest tick). Consumed by the metrics
/// exporter (`metrics::encode`); empty if the collector has never sampled.
pub fn latest_runners(conn: &Connection) -> Result<Vec<RunnerSample>> {
    let max_ts: Option<i64> =
        conn.query_row("SELECT max(ts) FROM runner_sample", [], |r| r.get(0))?;
    let Some(ts) = max_ts else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT ts, agent_id, name, org, liveness, current_run_id, cpu_pct, mem_bytes, uptime_s, dir, mem_current_bytes \
         FROM runner_sample WHERE ts = ?1",
    )?;
    let rows = stmt.query_map(params![ts], |r| {
        Ok(RunnerSample {
            ts: r.get(0)?,
            agent_id: r.get(1)?,
            name: r.get(2)?,
            org: r.get(3)?,
            liveness: Liveness::from_db(&r.get::<_, String>(4)?),
            current_run_id: r.get(5)?,
            cpu_pct: r.get::<_, Option<f64>>(6)?.map(|v| v as f32),
            mem_bytes: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
            uptime_s: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            dir: r.get(9)?,
            mem_current_bytes: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Current liveness + since-edge timestamp per runner, keyed by install `dir`.
/// Drives the "Idle/Active for <dur>" display.
pub fn runner_states(conn: &Connection) -> Result<HashMap<String, RunnerState>> {
    let mut stmt =
        conn.prepare_cached("SELECT dir, liveness, since_ts, last_seen_ts FROM runner_state")?;
    let rows = stmt.query_map([], |r| {
        let dir: String = r.get(0)?;
        Ok((
            dir.clone(),
            RunnerState {
                dir,
                liveness: Liveness::from_db(&r.get::<_, String>(1)?),
                since_ts: r.get(2)?,
                last_seen_ts: r.get(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The most recent host sample, if any (for the metrics exporter + banners).
pub fn latest_host(conn: &Connection) -> Result<Option<HostPoint>> {
    Ok(host_series(conn, 1)?.pop())
}

/// The oldest retained sample, or `None` when nothing has been sampled yet.
///
/// `runner_sample` alone, not a union across every `*_sample` table: they are
/// pruned together by `db prune`, and runners are the series that exists on
/// every deployment — a host with metrics disabled still samples runners. A
/// union would trade a covering-index probe for a scan per table to sharpen an
/// answer nobody reads that precisely.
///
/// The cheapness is the point. This fact was previously read out of
/// [`timeline::timeline`]'s `window.truncated_at`, which meant deriving every
/// edge across the caller's window: 8.13s for the 7 days `doctor` asked for, on
/// a 1.1 GiB / 8.1M-row database. `min(ts)` over `idx_runner_sample_ts` is a
/// covering-index search — 0.25ms on the same table.
pub fn retention(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row("SELECT min(ts) FROM runner_sample", [], |r| r.get(0))
        .map_err(Into::into)
}

/// Seeding helpers shared by all three producers' test modules.
///
/// In the parent because a seeded in-memory database is the same database
/// whichever producer's rows a test is about — copying it per child would let
/// the three drift apart while every test still passed.
#[cfg(test)]
mod fixtures {
    use rusqlite::{Connection, params};

    pub(super) fn mem_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        conn
    }

    pub(super) fn api_sample(
        conn: &Connection,
        ts: i64,
        org: &str,
        id: i64,
        online: i64,
        busy: i64,
    ) {
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (?1, ?2, ?3, 'r', ?4, ?5)",
            params![ts, id, org, online, busy],
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{api_sample, mem_db};
    use super::*;

    /// An empty store must answer "nothing yet" rather than a zero timestamp —
    /// `doctor` renders this, and 1970 is not a retention window.
    #[test]
    fn retention_of_an_empty_store_is_none_not_zero() {
        assert_eq!(retention(&mem_db()).unwrap(), None);
    }

    /// The OLDEST sample, not the newest, and unaffected by which runner or org
    /// wrote it: retention is a property of the store, not of any one series.
    #[test]
    fn retention_is_the_oldest_sample_across_every_runner() {
        let conn = mem_db();
        for (ts, name, org) in [
            (900, "b", "org-b"),
            (300, "a", "org-a"),
            (600, "c", "org-a"),
        ] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, cpu_pct, mem_bytes, dir) \
                 VALUES (?1, 7, ?2, ?3, 'idle', 1.0, 1024, '/srv/r7')",
                params![ts, name, org],
            )
            .unwrap();
        }
        assert_eq!(retention(&conn).unwrap(), Some(300));
    }

    #[test]
    fn history_is_chronological_and_limited() {
        let conn = mem_db();
        for ts in [100, 200, 300, 400] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, cpu_pct, mem_bytes, dir) \
                 VALUES (?1, 7, 'r', 'o', 'idle', ?2, ?3, '/srv/r7')",
                params![ts, (ts as f64) / 10.0, ts * 1000],
            )
            .unwrap();
        }
        let h = runner_history(&conn, "/srv/r7", 3).unwrap();
        // newest 3, oldest → newest
        assert_eq!(
            h.iter().map(|p| p.ts).collect::<Vec<_>>(),
            vec![200, 300, 400]
        );
        assert_eq!(h.last().unwrap().mem_bytes, Some(400_000));
        assert!(runner_history(&conn, "/srv/nobody", 10).unwrap().is_empty());
    }

    #[test]
    fn busy_series_counts_busy_and_online_per_tick() {
        let conn = mem_db();
        // tick 100: two idle, one busy, one offline → busy=1 online=3
        for (id, live) in [(1, "idle"), (2, "busy"), (3, "idle"), (4, "offline")] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness) \
                 VALUES (100, ?1, 'r', 'o', ?2)",
                params![id, live],
            )
            .unwrap();
        }
        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].busy, s[0].online), (1, 3));
    }

    /// A tick with local samples but no reconcile data must plot a GAP. Emitting
    /// 0 would draw "GitHub says nothing is online" for a fleet nobody asked
    /// GitHub about — inventing an outage instead of admitting ignorance.
    #[test]
    fn busy_series_plots_a_gap_not_a_zero_for_a_tick_without_api_data() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
             VALUES (100, 1, 'r', 'o', 'idle', '/d1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
             VALUES (200, 1, 'r', 'o', 'idle', '/d1')",
            [],
        )
        .unwrap();
        // Only tick 200 has a reconcile row.
        api_sample(&conn, 200, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 2);
        // Tick 100 predates every reading — a gap, and never a zero. A reading
        // is only ever carried FORWARD; inventing GitHub's opinion of a moment
        // before it was asked would be a different lie in the same family.
        assert!(s[0].github.is_none());
        let gh = s[1].github.unwrap();
        assert_eq!((gh.online, gh.known), (1, 1));
    }

    /// The bug this fixes: the two producers are independent threads on
    /// different periods (5s local, 60s API) that each stamp their own clock, so
    /// an exact-`ts` join matched only when both fired inside the same second.
    /// Every tick between two reconciles reported a gap, and the series' density
    /// tracked scheduler drift rather than whether GitHub data existed.
    #[test]
    fn busy_series_carries_the_newest_reading_at_or_before_each_tick() {
        let conn = mem_db();
        for ts in [100, 105, 110, 115] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (?1, 1, 'r', 'o', 'idle', '/d1')",
                params![ts],
            )
            .unwrap();
        }
        // One reconcile, landing on the first tick only.
        api_sample(&conn, 100, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 4);
        // All four ticks carry it: under the old exact-`ts` join only the first
        // did, so three of every four points vanished.
        for p in &s {
            let gh = p.github.expect("reading carried forward");
            assert_eq!((gh.online, gh.known), (1, 1));
        }
    }

    /// Carrying forward is only honest while the reading is fresh. Past
    /// `max_age` the series must decay into a gap — otherwise a dead reconcile
    /// thread draws a confident flat line forever, which is the failure the
    /// whole GitHub-view rework exists to prevent.
    #[test]
    fn busy_series_stops_carrying_a_reading_past_max_age() {
        let conn = mem_db();
        for ts in [100, 200, 400] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (?1, 1, 'r', 'o', 'idle', '/d1')",
                params![ts],
            )
            .unwrap();
        }
        api_sample(&conn, 100, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert!(s[0].github.is_some()); // age 0
        assert!(s[1].github.is_some()); // age 100, inside the window
        assert!(s[2].github.is_none()); // age 300, past it — a gap
    }

    /// The count speaks only for runners it has a reading for, and says so.
    /// A fleet holding an org GitHub is never asked about (a personal account
    /// has no org runner API) would otherwise draw a permanent divergence: the
    /// GitHub line sitting below the local one forever, for runners nobody ever
    /// asked about. `known` is what tells that silence apart from an outage.
    #[test]
    fn busy_series_counts_only_the_runners_it_has_a_reading_for() {
        let conn = mem_db();
        for (id, org) in [(1, "asked"), (2, "never-asked")] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (100, ?1, 'r', ?2, 'idle', ?3)",
                params![id, org, format!("/d{id}")],
            )
            .unwrap();
        }
        api_sample(&conn, 100, "asked", 1, 1, 0);
        // A runner GitHub knows about that this host does not run — another
        // machine in the same org. It must not inflate our count.
        api_sample(&conn, 100, "asked", 99, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        let gh = s[0].github.unwrap();
        assert_eq!(s[0].online, 2);
        assert_eq!((gh.online, gh.known), (1, 1));
    }

    #[test]
    fn host_series_chronological() {
        let conn = mem_db();
        for ts in [10, 20, 30] {
            conn.execute(
                "INSERT INTO host_sample (ts, load1, load5, mem_used, mem_total, tmp_bytes) \
                 VALUES (?1, 1.0, 1.0, 100, 200, ?2)",
                params![ts, ts * 5],
            )
            .unwrap();
        }
        let s = host_series(&conn, 2).unwrap();
        assert_eq!(s.iter().map(|p| p.ts).collect::<Vec<_>>(), vec![20, 30]);
        assert_eq!(s.last().unwrap().tmp_bytes, Some(150));
        assert_eq!(s[0].work_bytes, None);
    }

    #[test]
    fn latest_runners_uses_newest_tick() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO runner_sample (ts,agent_id,name,org,liveness) VALUES (100,1,'r1','o','idle')",
            [],
        )
        .unwrap();
        for (id, name, live) in [(1, "r1", "busy"), (2, "r2", "idle")] {
            conn.execute(
                "INSERT INTO runner_sample (ts,agent_id,name,org,liveness) VALUES (200,?1,?2,'o',?3)",
                params![id, name, live],
            )
            .unwrap();
        }
        let rows = latest_runners(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.ts == 200));
        let r1 = rows.iter().find(|r| r.agent_id == 1).unwrap();
        assert_eq!(r1.liveness, Liveness::Busy);
        assert!(latest_runners(&mem_db()).unwrap().is_empty());
    }
}
