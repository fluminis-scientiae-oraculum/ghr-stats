//! Server-side reads (the IPC server + the metrics exporter). The collector is
//! the only writer; readers open their own connection and rely on WAL for
//! contention-free concurrent reads. The TUI no longer opens the DB — in
//! Persistent mode it fetches these same shapes over the IPC socket, so the
//! query structs below double as the IPC wire payloads (hence the serde derives).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::shared::error::Result;
use crate::shared::models::{
    ApiReconcileState, ApiRunnerState, ApiState, BusyPoint, GhCount, GhView, HistPoint, HostPoint,
    JobRow, Liveness, PendingConclusion, RunnerSample, RunnerState,
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

/// Every tailed stream's persisted byte offset, keyed by stream id (the
/// per-runner event-log path). Empty on a fresh DB; a stream absent from the map
/// is tailed from 0. Loaded once at collector start to seed the hooks tailer.
pub fn ingest_offsets(conn: &Connection) -> Result<HashMap<String, u64>> {
    let mut stmt = conn.prepare_cached("SELECT stream, offset FROM ingest_offset")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Most recent jobs, newest first (by start, falling back to completion).
pub fn recent_jobs(conn: &Connection, limit: usize) -> Result<Vec<JobRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT runner_name, repo, job, started_at, completed_at, conclusion \
         FROM job_event ORDER BY COALESCE(started_at, completed_at) DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(JobRow {
            runner_name: r.get(0)?,
            repo: r.get(1)?,
            job: r.get(2)?,
            started_at: r.get(3)?,
            completed_at: r.get(4)?,
            conclusion: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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

/// GitHub's latest view of every runner, keyed by `(org, agent_id)`. GitHub's
/// `agent_id` is unique only within an org, so the org must be part of the key —
/// two runners in different orgs can share an id.
///
/// Latest **per runner**, not per global tick. The old query took `max(ts)` over
/// the whole table and returned only rows at that instant, which had two bad
/// consequences: an org that failed a single reconcile had no row at the newest
/// ts, so its runners' GitHub series *vanished* from the export rather than
/// reporting a value; and there was no age bound at all, so if the reconcile
/// thread died the last successful tick was served as current forever.
///
/// `max_age` closes both. Each runner keeps its own most recent reading, and
/// anything older than the window is returned as [`GhView::Stale`] — visibly
/// aged, never silently presented as live.
pub fn latest_api_runners(
    conn: &Connection,
    now: i64,
    max_age: u64,
) -> Result<HashMap<(String, i64), GhView>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.org, s.agent_id, s.online, s.busy, s.ts \
         FROM api_runner_sample s \
         JOIN (SELECT org, agent_id, max(ts) AS ts \
               FROM api_runner_sample GROUP BY org, agent_id) m \
           ON m.org = s.org AND m.agent_id = s.agent_id AND m.ts = s.ts",
    )?;
    let rows = stmt.query_map([], |r| {
        let ts: i64 = r.get(4)?;
        let state = ApiState {
            online: r.get::<_, i64>(2)? != 0,
            busy: r.get::<_, i64>(3)? != 0,
        };
        let view = GhView::observed(state, now - ts, max_age);
        Ok(((r.get::<_, String>(0)?, r.get::<_, i64>(1)?), view))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The persisted GitHub-side liveness edges, keyed by `(org, agent_id)`. Feeds
/// `ghr_runner_github_offline_seconds` — the duration an alert debounces on.
pub fn api_runner_states(conn: &Connection) -> Result<HashMap<(String, i64), ApiRunnerState>> {
    let mut stmt = conn.prepare_cached(
        "SELECT org, agent_id, online, since_ts, last_seen_ts FROM api_runner_state",
    )?;
    let rows = stmt.query_map([], |r| {
        let org: String = r.get(0)?;
        let agent_id: i64 = r.get(1)?;
        Ok((
            (org.clone(), agent_id),
            ApiRunnerState {
                org,
                agent_id,
                online: r.get::<_, i64>(2)? != 0,
                since_ts: r.get(3)?,
                last_seen_ts: r.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Per-org reconcile health, newest state per org. Empty before the first
/// reconcile tick.
pub fn api_reconcile_states(conn: &Connection) -> Result<Vec<ApiReconcileState>> {
    let mut stmt = conn.prepare_cached(
        "SELECT org, last_ok_ts, last_try_ts, ok, http_status, error_kind, configured \
         FROM api_reconcile_state ORDER BY org",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ApiReconcileState {
            org: r.get(0)?,
            last_ok_ts: r.get(1)?,
            last_try_ts: r.get(2)?,
            ok: r.get::<_, i64>(3)? != 0,
            http_status: r.get::<_, Option<i64>>(4)?.map(|v| v as u16),
            error_kind: r.get(5)?,
            configured: r.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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

/// Completed jobs whose pass/fail conclusion has not yet been resolved from the
/// API — the reconcile's work-list. Newest completions first; bounded so a large
/// backlog is drained a batch at a time. Only rows that carry an `org` + `repo`
/// (the hook fills these) are returned, since both are needed to call the API.
pub fn jobs_awaiting_conclusion(conn: &Connection, limit: usize) -> Result<Vec<PendingConclusion>> {
    let mut stmt = conn.prepare_cached(
        "SELECT org, repo, run_id, run_attempt, job, runner_name FROM job_event \
         WHERE completed_at IS NOT NULL AND conclusion IS NULL \
               AND org <> '' AND repo <> '' \
         ORDER BY completed_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(PendingConclusion {
            org: r.get(0)?,
            repo: r.get(1)?,
            run_id: r.get(2)?,
            run_attempt: r.get(3)?,
            job: r.get(4)?,
            runner_name: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// `(total job_event rows, in-flight rows)` — jobs whose `completed_at` is NULL.
pub fn job_counts(conn: &Connection) -> Result<(i64, i64)> {
    conn.query_row(
        "SELECT count(*), COALESCE(SUM(completed_at IS NULL), 0) FROM job_event",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(Into::into)
}

/// The runner's most recent job (running OR last completed), for the detail
/// panel's "job" line. Newest by start, falling back to completion. Returns the
/// full row so the caller can render "running Xs" vs "<conclusion>, Xs ago" —
/// unlike an in-flight-only query, an idle runner still shows its last job rather
/// than a bare "—". Local hook timing — immediate.
pub fn latest_job(conn: &Connection, runner_name: &str) -> Result<Option<JobRow>> {
    conn.query_row(
        "SELECT runner_name, repo, job, started_at, completed_at, conclusion \
         FROM job_event WHERE runner_name = ?1 \
         ORDER BY COALESCE(started_at, completed_at) DESC LIMIT 1",
        params![runner_name],
        |r| {
            Ok(JobRow {
                runner_name: r.get(0)?,
                repo: r.get(1)?,
                job: r.get(2)?,
                started_at: r.get(3)?,
                completed_at: r.get(4)?,
                conclusion: r.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        conn
    }

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

    fn api_sample(conn: &Connection, ts: i64, org: &str, id: i64, online: i64, busy: i64) {
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (?1, ?2, ?3, 'r', ?4, ?5)",
            params![ts, id, org, online, busy],
        )
        .unwrap();
    }

    #[test]
    fn latest_api_runners_takes_the_newest_row_per_runner() {
        let conn = mem_db();
        api_sample(&conn, 100, "o", 1, 1, 0); // older reading for r1
        api_sample(&conn, 200, "o", 1, 1, 1); // newer: r1 now busy
        api_sample(&conn, 200, "o", 2, 0, 0); // r2 offline

        let m = latest_api_runners(&conn, 200, 180).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[&("o".to_string(), 1)].busy(), Some(true));
        assert_eq!(m[&("o".to_string(), 1)].online(), Some(true));
        assert_eq!(m[&("o".to_string(), 2)].online(), Some(false));
        assert!(latest_api_runners(&mem_db(), 200, 180).unwrap().is_empty());
    }

    /// The regression that made the 2026-07-25 outage undiagnosable: with a
    /// global `max(ts)`, an org absent from the newest tick had no row at that
    /// instant, so its runners' GitHub series VANISHED from the export rather
    /// than reporting a value. A missing series and a zero are indistinguishable
    /// to a scrape, which is exactly the wrong answer at 2am.
    #[test]
    fn an_org_missing_from_the_newest_tick_keeps_its_last_reading() {
        let conn = mem_db();
        // Tick 100: both orgs answered.
        api_sample(&conn, 100, "org-a", 1, 1, 0);
        api_sample(&conn, 100, "org-b", 1, 1, 0);
        // Tick 200: only org-a answered (org-b's token broke).
        api_sample(&conn, 200, "org-a", 1, 1, 0);

        let m = latest_api_runners(&conn, 200, 180).unwrap();
        // org-b is still PRESENT, carrying its older reading and an honest age.
        let b = m[&("org-b".to_string(), 1)];
        assert_eq!(b.online(), Some(true));
        assert!(matches!(b, GhView::Fresh { age_s: 100, .. }));
        // org-a is current.
        assert!(matches!(
            m[&("org-a".to_string(), 1)],
            GhView::Fresh { age_s: 0, .. }
        ));
    }

    /// Past the window a reading is reported as stale, never served as current.
    /// Without this a dead reconcile thread kept exporting confident values
    /// forever.
    #[test]
    fn a_reading_older_than_max_age_is_stale_not_live() {
        let conn = mem_db();
        api_sample(&conn, 100, "o", 1, 1, 0);

        // 60s later, inside a 180s window: still trustworthy.
        let fresh = latest_api_runners(&conn, 160, 180).unwrap();
        assert!(matches!(fresh[&("o".to_string(), 1)], GhView::Fresh { .. }));

        // 6 hours later: we still have the row, but it is not evidence of
        // anything current — online() must go unknown rather than stay `true`.
        let old = latest_api_runners(&conn, 100 + 21_600, 180).unwrap();
        let v = old[&("o".to_string(), 1)];
        assert!(matches!(v, GhView::Stale { .. }));
        assert_eq!(v.online(), None);
        assert!(matches!(v, GhView::Stale { age_s: 21_600 }));
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

    #[test]
    fn ingest_offsets_loads_every_stream_and_is_empty_on_fresh_db() {
        let conn = mem_db();
        assert!(ingest_offsets(&conn).unwrap().is_empty());
        for (stream, off) in [
            ("/srv/runners/runner-01/.ghr-stats-events.ndjson", 40),
            ("/srv/runners/runner-02/.ghr-stats-events.ndjson", 128),
        ] {
            conn.execute(
                "INSERT INTO ingest_offset (stream, offset) VALUES (?1, ?2)",
                params![stream, off],
            )
            .unwrap();
        }
        let m = ingest_offsets(&conn).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["/srv/runners/runner-01/.ghr-stats-events.ndjson"], 40);
        assert_eq!(m["/srv/runners/runner-02/.ghr-stats-events.ndjson"], 128);
    }

    #[test]
    fn jobs_awaiting_conclusion_only_completed_null_with_org_and_repo() {
        let conn = mem_db();
        // completed, no conclusion, has org+repo → the work-list.
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,repo,org,runner_name,started_at,completed_at) \
             VALUES (1,1,'build','o/x','o','r',10,20)",
            [],
        )
        .unwrap();
        // completed but already concluded → excluded.
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,repo,org,runner_name,started_at,completed_at,conclusion) \
             VALUES (2,1,'t','o/x','o','r',10,20,'success')",
            [],
        )
        .unwrap();
        // still running → excluded.
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,repo,org,runner_name,started_at) \
             VALUES (3,1,'run','o/x','o','r',10)",
            [],
        )
        .unwrap();
        // completed but no org/repo (can't call the API) → excluded.
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,runner_name,started_at,completed_at) \
             VALUES (4,1,'x','r',10,20)",
            [],
        )
        .unwrap();

        let p = jobs_awaiting_conclusion(&conn, 10).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            (
                p[0].run_id,
                p[0].job.as_str(),
                p[0].repo.as_str(),
                p[0].org.as_str()
            ),
            (1, "build", "o/x", "o")
        );
    }

    #[test]
    fn latest_job_is_the_most_recent_running_or_done() {
        let conn = mem_db();
        // Older, completed.
        conn.execute(
            "INSERT INTO job_event (run_id,job,repo,runner_name,started_at,completed_at) \
             VALUES (1,'a','o/x','r',100,150)",
            [],
        )
        .unwrap();
        // Newer, still running.
        conn.execute(
            "INSERT INTO job_event (run_id,job,repo,runner_name,started_at) \
             VALUES (2,'b','o/y','r',200)",
            [],
        )
        .unwrap();
        let j = latest_job(&conn, "r").unwrap().unwrap();
        assert_eq!((j.job.as_str(), j.repo.as_str()), ("b", "o/y"));
        assert!(j.completed_at.is_none()); // running

        // Once it completes it is STILL the latest — an idle runner shows its last
        // job, not a bare "—" (the whole point of latest_job vs in-flight-only).
        conn.execute("UPDATE job_event SET completed_at=260 WHERE run_id=2", [])
            .unwrap();
        let j = latest_job(&conn, "r").unwrap().unwrap();
        assert_eq!((j.job.as_str(), j.completed_at), ("b", Some(260)));

        assert!(latest_job(&conn, "nobody").unwrap().is_none());
    }
}
