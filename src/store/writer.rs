//! Collector-side writes. One transaction per tick keeps a sample atomic.

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::hooks::ingest::HookEvent;
use crate::model::{ApiRunnerRow, HostSample, RunnerSample};

/// Persist one tick: all runner rows plus the host row, atomically.
pub fn write_local(
    conn: &mut Connection,
    runners: &[RunnerSample],
    host: &HostSample,
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO runner_sample \
             (ts, agent_id, name, org, liveness, current_run_id, cpu_pct, mem_bytes, uptime_s) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for r in runners {
            stmt.execute(params![
                r.ts,
                r.agent_id,
                r.name,
                r.org,
                r.liveness.as_str(),
                r.current_run_id,
                r.cpu_pct.map(|v| v as f64),
                r.mem_bytes.map(|v| v as i64),
                r.uptime_s.map(|v| v as i64),
            ])?;
        }
    }
    let numa_json = serde_json::to_string(&host.numa).unwrap_or_else(|_| "[]".to_string());
    tx.execute(
        "INSERT INTO host_sample \
         (ts, load1, load5, mem_used, mem_total, numa_json, work_bytes, tmp_bytes, root_free) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            host.ts,
            host.load1,
            host.load5,
            host.mem_used as i64,
            host.mem_total as i64,
            numa_json,
            host.work_bytes.map(|v| v as i64),
            host.tmp_bytes.map(|v| v as i64),
            host.root_free.map(|v| v as i64),
        ],
    )?;

    // Edge-detect liveness: reset `since_ts` only when a runner's liveness
    // actually changes, so the TUI can show "Idle/Active for <dur>". One row
    // per runner; `last_seen_ts` always advances. Pure-SQL edge detection —
    // the single-writer connection makes the read-compare-write race-free.
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO runner_state (agent_id, liveness, since_ts, last_seen_ts) \
             VALUES (?1, ?2, ?3, ?3) \
             ON CONFLICT(agent_id) DO UPDATE SET \
                 since_ts = CASE WHEN runner_state.liveness = excluded.liveness \
                                 THEN runner_state.since_ts ELSE excluded.since_ts END, \
                 liveness = excluded.liveness, \
                 last_seen_ts = excluded.last_seen_ts",
        )?;
        for r in runners {
            stmt.execute(params![r.agent_id, r.liveness.as_str(), r.ts])?;
        }
    }

    tx.commit()?;
    Ok(())
}

/// Delete time-series samples older than `cutoff_ts`. `job_event` is kept (low
/// volume, high value). Returns the number of rows removed. Safe to run while
/// the collector writes — WAL handles the concurrency.
pub fn prune(conn: &mut Connection, cutoff_ts: i64) -> Result<usize> {
    const SAMPLE_TABLES: [&str; 4] = [
        "runner_sample",
        "host_sample",
        "api_runner_sample",
        "queue_sample",
    ];
    let tx = conn.transaction()?;
    let mut removed = 0;
    for table in SAMPLE_TABLES {
        // Table names are fixed literals — no injection surface.
        removed += tx.execute(
            &format!("DELETE FROM {table} WHERE ts < ?1"),
            params![cutoff_ts],
        )?;
    }
    tx.commit()?;
    Ok(removed)
}

/// Persist one GitHub API reconcile tick (all orgs share a single `ts`).
pub fn write_api_runners(conn: &mut Connection, ts: i64, rows: &[ApiRunnerRow]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for r in rows {
            stmt.execute(params![
                ts,
                r.agent_id,
                r.org,
                r.name,
                r.online as i64,
                r.busy as i64
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Upsert hook job events and advance the ingest offset, atomically. `started`
/// and `completed` for the same job key merge into one row (each fills the
/// timestamp it carries without clobbering the other).
pub fn apply_hook_events(conn: &mut Connection, events: &[HookEvent], offset: u64) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO job_event \
             (run_id, run_attempt, job, repo, org, runner_name, started_at, completed_at, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'hook') \
             ON CONFLICT(run_id, run_attempt, job, runner_name) DO UPDATE SET \
                 started_at   = COALESCE(excluded.started_at,   job_event.started_at), \
                 completed_at = COALESCE(excluded.completed_at, job_event.completed_at), \
                 repo = excluded.repo, org = excluded.org",
        )?;
        for e in events {
            let org = e.repo.split('/').next().unwrap_or("").to_string();
            let (started, completed) = match e.phase.as_str() {
                "started" => (Some(e.ts), None),
                "completed" => (None, Some(e.ts)),
                _ => (None, None),
            };
            stmt.execute(params![
                e.run_id,
                e.run_attempt,
                e.job,
                e.repo,
                org,
                e.runner,
                started,
                completed,
            ])?;
        }
    }
    tx.execute(
        "INSERT INTO ingest_offset (stream, offset) VALUES ('hooks', ?1) \
         ON CONFLICT(stream) DO UPDATE SET offset = excluded.offset",
        params![offset as i64],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn hook_events_merge_started_and_completed() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema_for_test(&mut conn);
        let started = HookEvent {
            phase: "started".into(),
            ts: 1000,
            repo: "example-org/foo".into(),
            run_id: 7,
            run_attempt: 1,
            job: "build".into(),
            runner: "r0".into(),
        };
        let mut completed = started.clone();
        completed.phase = "completed".into();
        completed.ts = 1050;

        apply_hook_events(&mut conn, &[started], 10).unwrap();
        apply_hook_events(&mut conn, &[completed], 20).unwrap();

        let (s, c, org): (i64, i64, String) = conn
            .query_row(
                "SELECT started_at, completed_at, org FROM job_event WHERE run_id=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((s, c), (1000, 1050)); // both timestamps present on one row
        assert_eq!(org, "example-org");
        let off: i64 = conn
            .query_row(
                "SELECT offset FROM ingest_offset WHERE stream='hooks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(off, 20);
    }

    #[test]
    fn prune_removes_old_samples_but_keeps_job_event() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema_for_test(&mut conn);
        conn.execute(
            "INSERT INTO runner_sample (ts,agent_id,name,org,liveness) VALUES (100,1,'r','o','idle')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runner_sample (ts,agent_id,name,org,liveness) VALUES (500,1,'r','o','idle')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO host_sample (ts,load1,load5,mem_used,mem_total) VALUES (100,1.0,1.0,1,2)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO job_event (run_id) VALUES (42)", [])
            .unwrap();

        // Cutoff 300 removes the two ts=100 rows; keeps ts=500 and job_event.
        let removed = prune(&mut conn, 300).unwrap();
        assert_eq!(removed, 2);
        let runners: i64 = conn
            .query_row("SELECT count(*) FROM runner_sample", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runners, 1);
        let jobs: i64 = conn
            .query_row("SELECT count(*) FROM job_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(jobs, 1);
    }

    #[test]
    fn runner_state_tracks_liveness_edges() {
        use crate::model::Liveness;

        let mut conn = Connection::open_in_memory().unwrap();
        crate::store::schema_for_test(&mut conn);
        let host = HostSample {
            ts: 0,
            load1: 0.0,
            load5: 0.0,
            mem_used: 0,
            mem_total: 0,
            numa: vec![],
            work_bytes: None,
            tmp_bytes: None,
            root_free: None,
        };
        let sample = |ts, live| RunnerSample {
            ts,
            agent_id: 1,
            name: "r".into(),
            org: "o".into(),
            liveness: live,
            current_run_id: None,
            cpu_pct: None,
            mem_bytes: None,
            uptime_s: None,
        };

        // Two idle ticks: since_ts pins to the FIRST one (no edge on the second).
        write_local(&mut conn, &[sample(100, Liveness::Idle)], &host).unwrap();
        write_local(&mut conn, &[sample(200, Liveness::Idle)], &host).unwrap();
        let (live, since): (String, i64) = conn
            .query_row(
                "SELECT liveness, since_ts FROM runner_state WHERE agent_id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((live.as_str(), since), ("idle", 100));

        // A liveness change moves since_ts to the change time; last_seen advances.
        write_local(&mut conn, &[sample(300, Liveness::Busy)], &host).unwrap();
        let (live, since, seen): (String, i64, i64) = conn
            .query_row(
                "SELECT liveness, since_ts, last_seen_ts FROM runner_state WHERE agent_id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((live.as_str(), since, seen), ("busy", 300, 300));
    }
}
