//! Server-side reads for the IPC server and metrics exporter, each on its own WAL connection.
use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::shared::error::Result;
use crate::shared::models::{BusyPoint, GhCount, HistPoint, HostPoint, RunnerSample, RunnerState};

/// Edge derivations and window assembly for the `timeline` query.
pub mod timeline;

pub use timeline::timeline;

mod github;
mod jobs;

pub use github::{api_reconcile_states, api_runner_states, latest_api_runners};
pub use jobs::{ingest_offsets, job_counts, jobs_awaiting_conclusion, latest_job, recent_jobs};

/// Newest `limit` samples, oldest first. An unknown `dir` answers empty at once rather
/// than scanning the whole table for it.
pub fn runner_history(conn: &Connection, dir: &str, limit: usize) -> Result<Vec<HistPoint>> {
    let known: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM runner_state WHERE dir = ?1",
            params![dir],
            |r| r.get(0),
        )
        .optional()?;
    if known.is_none() {
        return Ok(Vec::new());
    }
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

/// Newest `limit` host samples, oldest first.
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

/// Occupancy per tick, oldest first. Each runner's GitHub reading is the newest at or
/// before the tick, within `max_age`: the local and API threads stamp independent clocks,
/// so an exact-`ts` join misses.
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
            // `gh_online` is NULL exactly when `gh_known == 0`.
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

pub fn latest_runners(conn: &Connection) -> Result<Vec<RunnerSample>> {
    let max_ts: Option<i64> =
        conn.query_row("SELECT max(ts) FROM runner_sample", [], |r| r.get(0))?;
    let Some(ts) = max_ts else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT ts, agent_id, name, org, liveness, cpu_pct, mem_bytes, uptime_s, dir, mem_current_bytes \
         FROM runner_sample WHERE ts = ?1",
    )?;
    let rows = stmt.query_map(params![ts], |r| {
        Ok(RunnerSample {
            ts: r.get(0)?,
            agent_id: r.get(1)?,
            name: r.get(2)?,
            org: r.get(3)?,
            liveness: r.get(4)?,
            cpu_pct: r.get::<_, Option<f64>>(5)?.map(|v| v as f32),
            mem_bytes: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            uptime_s: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
            dir: r.get(8)?,
            mem_current_bytes: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn runner_states(conn: &Connection) -> Result<HashMap<String, RunnerState>> {
    let mut stmt =
        conn.prepare_cached("SELECT dir, liveness, since_ts, last_seen_ts FROM runner_state")?;
    let rows = stmt.query_map([], |r| {
        let dir: String = r.get(0)?;
        Ok((
            dir.clone(),
            RunnerState {
                dir,
                liveness: r.get(1)?,
                since_ts: r.get(2)?,
                last_seen_ts: r.get(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn latest_host(conn: &Connection) -> Result<Option<HostPoint>> {
    Ok(host_series(conn, 1)?.pop())
}

/// Oldest retained sample. `runner_sample` alone suffices since `db prune` prunes every sample
/// table together, and `min(ts)` stays a covering-index probe.
pub fn retention(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row("SELECT min(ts) FROM runner_sample", [], |r| r.get(0))
        .map_err(Into::into)
}

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
    use crate::shared::models::Liveness;

    #[test]
    fn retention_of_an_empty_store_is_none_not_zero() {
        assert_eq!(retention(&mem_db()).unwrap(), None);
    }

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
        conn.execute(
            "INSERT INTO runner_state (dir, liveness, since_ts, last_seen_ts) \
             VALUES ('/srv/r7', 'idle', 100, 400)",
            [],
        )
        .unwrap();
        let h = runner_history(&conn, "/srv/r7", 3).unwrap();
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
        api_sample(&conn, 200, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 2);
        assert!(s[0].github.is_none());
        let gh = s[1].github.unwrap();
        assert_eq!((gh.online, gh.known), (1, 1));
    }

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
        api_sample(&conn, 100, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 4);
        for p in &s {
            let gh = p.github.expect("reading carried forward");
            assert_eq!((gh.online, gh.known), (1, 1));
        }
    }

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
        // Another host's runner in the same org must not inflate our count.
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
