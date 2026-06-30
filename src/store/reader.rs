//! TUI-side reads. The collector is the only writer; readers open their own
//! connection and rely on WAL for contention-free concurrent reads.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// A recent job, joined from hook timing + (eventually) API conclusion.
#[derive(Debug, Clone)]
pub struct JobRow {
    pub runner_name: String,
    pub repo: String,
    pub job: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub conclusion: Option<String>,
}

/// GitHub's view of one runner (from the latest reconcile tick).
#[derive(Debug, Clone, Copy)]
pub struct ApiState {
    pub online: bool,
    pub busy: bool,
}

/// One historical runner sample, for sparklines.
#[derive(Debug, Clone)]
pub struct HistPoint {
    pub ts: i64,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
}

/// One host time-series point.
#[derive(Debug, Clone)]
pub struct HostPoint {
    pub ts: i64,
    pub load1: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub tmp_bytes: Option<u64>,
    pub work_bytes: Option<u64>,
}

/// One fleet-occupancy point: how many runners were busy / online at a tick.
#[derive(Debug, Clone)]
pub struct BusyPoint {
    pub ts: i64,
    pub busy: u32,
    pub online: u32,
}

/// The most recent `limit` samples for a runner, returned oldest → newest so
/// they can be fed straight into a left-to-right sparkline.
pub fn runner_history(conn: &Connection, agent_id: i64, limit: usize) -> Result<Vec<HistPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, cpu_pct, mem_bytes FROM runner_sample \
         WHERE agent_id = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit as i64], |r| {
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

/// The persisted byte offset for a tailed stream (0 if never recorded).
pub fn ingest_offset(conn: &Connection, stream: &str) -> Result<u64> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT offset FROM ingest_offset WHERE stream = ?1",
            params![stream],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.unwrap_or(0).max(0) as u64)
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
        "SELECT ts, load1, mem_used, mem_total, tmp_bytes, work_bytes FROM host_sample \
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
        })
    })?;
    let mut out: Vec<HostPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

/// GitHub's latest view of every runner, keyed by `agent_id`. Empty if the
/// API reconcile has never run (e.g. no token, or daemon not running).
pub fn latest_api_runners(conn: &Connection) -> Result<HashMap<i64, ApiState>> {
    let max_ts: Option<i64> =
        conn.query_row("SELECT max(ts) FROM api_runner_sample", [], |r| r.get(0))?;
    let Some(ts) = max_ts else {
        return Ok(HashMap::new());
    };
    let mut stmt =
        conn.prepare_cached("SELECT agent_id, online, busy FROM api_runner_sample WHERE ts = ?1")?;
    let rows = stmt.query_map(params![ts], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            ApiState {
                online: r.get::<_, i64>(1)? != 0,
                busy: r.get::<_, i64>(2)? != 0,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Fleet occupancy per tick (busy and online counts), oldest → newest.
pub fn busy_series(conn: &Connection, limit: usize) -> Result<Vec<BusyPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, \
                SUM(liveness = 'busy') AS busy, \
                SUM(liveness <> 'offline') AS online \
         FROM runner_sample GROUP BY ts ORDER BY ts DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(BusyPoint {
            ts: r.get(0)?,
            busy: r.get::<_, i64>(1)? as u32,
            online: r.get::<_, i64>(2)? as u32,
        })
    })?;
    let mut out: Vec<BusyPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema_for_test(&mut conn);
        conn
    }

    #[test]
    fn history_is_chronological_and_limited() {
        let conn = mem_db();
        for ts in [100, 200, 300, 400] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, cpu_pct, mem_bytes) \
                 VALUES (?1, 7, 'r', 'o', 'idle', ?2, ?3)",
                params![ts, (ts as f64) / 10.0, ts * 1000],
            )
            .unwrap();
        }
        let h = runner_history(&conn, 7, 3).unwrap();
        // newest 3, oldest → newest
        assert_eq!(
            h.iter().map(|p| p.ts).collect::<Vec<_>>(),
            vec![200, 300, 400]
        );
        assert_eq!(h.last().unwrap().mem_bytes, Some(400_000));
        assert!(runner_history(&conn, 999, 10).unwrap().is_empty());
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
        let s = busy_series(&conn, 10).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].busy, s[0].online), (1, 3));
    }

    #[test]
    fn latest_api_runners_uses_newest_tick_only() {
        let conn = mem_db();
        // older tick
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (100, 1, 'o', 'r1', 1, 0)",
            [],
        )
        .unwrap();
        // newest tick: r1 now busy, r2 offline
        for (id, online, busy) in [(1, 1, 1), (2, 0, 0)] {
            conn.execute(
                "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
                 VALUES (200, ?1, 'o', 'r', ?2, ?3)",
                params![id, online, busy],
            )
            .unwrap();
        }
        let m = latest_api_runners(&conn).unwrap();
        assert_eq!(m.len(), 2);
        assert!(m[&1].busy && m[&1].online);
        assert!(!m[&2].online);
        assert!(latest_api_runners(&mem_db()).unwrap().is_empty());
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
}
