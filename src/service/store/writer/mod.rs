//! Collector-side writes. One transaction per tick keeps a sample atomic.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::models::{HostSample, RunnerSample};

mod github;
mod jobs;

pub use github::write_api_runners;
pub use jobs::{apply_hook_events, apply_job_conclusions};

pub fn write_local(
    conn: &mut Connection,
    runners: &[RunnerSample],
    host: &HostSample,
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO runner_sample \
             (ts, agent_id, name, org, liveness, cpu_pct, mem_bytes, uptime_s, dir, mem_current_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for r in runners {
            stmt.execute(params![
                r.ts,
                r.agent_id,
                r.name,
                r.org,
                r.liveness.as_str(),
                r.cpu_pct.map(|v| v as f64),
                r.mem_bytes.map(|v| v as i64),
                r.uptime_s.map(|v| v as i64),
                r.dir,
                r.mem_current_bytes.map(|v| v as i64),
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

    // `since_ts` moves only on a liveness change. The read-compare-write is race-free
    // because this is the only writer connection.
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO runner_state (dir, liveness, since_ts, last_seen_ts) \
             VALUES (?1, ?2, ?3, ?3) \
             ON CONFLICT(dir) DO UPDATE SET \
                 since_ts = CASE WHEN runner_state.liveness = excluded.liveness \
                                 THEN runner_state.since_ts ELSE excluded.since_ts END, \
                 liveness = excluded.liveness, \
                 last_seen_ts = excluded.last_seen_ts",
        )?;
        for r in runners {
            stmt.execute(params![r.dir, r.liveness.as_str(), r.ts])?;
        }
    }

    tx.commit()?;
    Ok(())
}

/// Delete up to `batch` samples older than `cutoff_ts` from each time-series table in one
/// transaction; returns how many went. Keeps `job_event`. A new time-series table must be
/// added to `SAMPLE_TABLES`.
pub fn prune_batch(conn: &mut Connection, cutoff_ts: i64, batch: usize) -> Result<usize> {
    const SAMPLE_TABLES: [&str; 5] = [
        "runner_sample",
        "host_sample",
        "api_runner_sample",
        "queue_sample",
        "api_reconcile_sample",
    ];
    let tx = conn.transaction()?;
    let mut removed = 0;
    for table in SAMPLE_TABLES {
        removed += tx.execute(
            &format!(
                "DELETE FROM {table} WHERE rowid IN \
                 (SELECT rowid FROM {table} WHERE ts < ?1 LIMIT ?2)"
            ),
            params![cutoff_ts, batch as i64],
        )?;
    }
    tx.commit()?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn prune_removes_old_samples_but_keeps_job_event() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
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

        let removed = prune_batch(&mut conn, 300, 1000).unwrap();
        assert_eq!(prune_batch(&mut conn, 300, 1000).unwrap(), 0);
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
        use crate::shared::models::Liveness;

        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
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
            dir: "/srv/r1".into(),
            name: "r".into(),
            org: "o".into(),
            liveness: live,
            cpu_pct: None,
            mem_bytes: None,
            mem_current_bytes: None,
            uptime_s: None,
        };

        write_local(&mut conn, &[sample(100, Liveness::Idle)], &host).unwrap();
        write_local(&mut conn, &[sample(200, Liveness::Idle)], &host).unwrap();
        let (live, since): (String, i64) = conn
            .query_row(
                "SELECT liveness, since_ts FROM runner_state WHERE dir='/srv/r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((live.as_str(), since), ("idle", 100));

        write_local(&mut conn, &[sample(300, Liveness::Busy)], &host).unwrap();
        let (live, since, seen): (String, i64, i64) = conn
            .query_row(
                "SELECT liveness, since_ts, last_seen_ts FROM runner_state WHERE dir='/srv/r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((live.as_str(), since, seen), ("busy", 300, 300));
    }
}
