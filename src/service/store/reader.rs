//! Server-side reads (the IPC server + the metrics exporter). The collector is
//! the only writer; readers open their own connection and rely on WAL for
//! contention-free concurrent reads. The TUI no longer opens the DB — in
//! Persistent mode it fetches these same shapes over the IPC socket, so the
//! query structs below double as the IPC wire payloads (hence the serde derives).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::shared::error::Result;
use crate::shared::models::{
    ApiState, BusyPoint, HistPoint, HostPoint, JobRow, Liveness, PendingConclusion, RunnerSample,
    RunnerState,
};

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

/// Every runner's most recent sample (the latest tick). Consumed by the metrics
/// exporter (`metrics::encode`); empty if the collector has never sampled.
pub fn latest_runners(conn: &Connection) -> Result<Vec<RunnerSample>> {
    let max_ts: Option<i64> =
        conn.query_row("SELECT max(ts) FROM runner_sample", [], |r| r.get(0))?;
    let Some(ts) = max_ts else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT ts, agent_id, name, org, liveness, current_run_id, cpu_pct, mem_bytes, uptime_s \
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
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Current liveness + since-edge timestamp per runner, keyed by `agent_id`.
/// Drives the "Idle/Active for <dur>" display.
pub fn runner_states(conn: &Connection) -> Result<HashMap<i64, RunnerState>> {
    let mut stmt =
        conn.prepare_cached("SELECT agent_id, liveness, since_ts, last_seen_ts FROM runner_state")?;
    let rows = stmt.query_map([], |r| {
        let agent_id: i64 = r.get(0)?;
        Ok((
            agent_id,
            RunnerState {
                agent_id,
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
        for (stream, off) in [("/srv/runners/runner-01/.ghr-stats-events.ndjson", 40), ("/srv/runners/runner-02/.ghr-stats-events.ndjson", 128)] {
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
            (p[0].run_id, p[0].job.as_str(), p[0].repo.as_str(), p[0].org.as_str()),
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
