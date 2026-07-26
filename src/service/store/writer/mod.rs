//! Collector-side writes. One transaction per tick keeps a sample atomic.
//!
//! Cut by WHOSE ROWS THESE ARE — the mirror of the [`super::reader`] split, and
//! the same three producers `serve` runs as separate threads:
//!
//! - **this file** — what the collector SAMPLED off this machine: the runner and
//!   host ticks, and the local liveness edge they drive.
//! - [`github`] — what GITHUB said, from the reconcile thread's `api_*` writes.
//! - [`jobs`] — what the RUNNERS' HOOKS reported: the ingested event log, the
//!   tailer's offset into it, and the conclusion write-back that fills a column
//!   the hook left NULL.
//!
//! Ownership, note, and not which thread makes the call. [`jobs`] holds
//! `apply_job_conclusions` even though the reconcile thread is what calls it,
//! because the rows it updates are the hooks' — and history agrees: that function
//! has changed twice, both times alongside the hook write and never alongside
//! [`github`].
//!
//! [`prune`] stays here for the opposite reason: it belongs to NO producer. It
//! deletes across every sample table, so its `SAMPLE_TABLES` list has to name
//! what all three children write — which is a thing a parent may know and a
//! sibling may not. `0.2.0` is the proof: adding `api_reconcile_sample` was one
//! commit touching both [`github`]'s writes and this list, the only edit that has
//! ever spanned two of these groups. `entrypoint` drives it on the retention
//! timer, not `serve` on a producer thread.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::models::{HostSample, RunnerSample};

mod github;
mod jobs;

pub use github::write_api_runners;
pub use jobs::{apply_hook_events, apply_job_conclusions};

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
             (ts, agent_id, name, org, liveness, current_run_id, cpu_pct, mem_bytes, uptime_s, dir, mem_current_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

    // Edge-detect liveness: reset `since_ts` only when a runner's liveness
    // actually changes, so the TUI can show "Idle/Active for <dur>". One row
    // per runner; `last_seen_ts` always advances. Pure-SQL edge detection —
    // the single-writer connection makes the read-compare-write race-free.
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

/// Delete time-series samples older than `cutoff_ts`. `job_event` is kept (low
/// volume, high value). Returns the number of rows removed. Safe to run while
/// the collector writes — WAL handles the concurrency.
pub fn prune(conn: &mut Connection, cutoff_ts: i64) -> Result<usize> {
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
        // Table names are fixed literals — no injection surface.
        removed += tx.execute(
            &format!("DELETE FROM {table} WHERE ts < ?1"),
            params![cutoff_ts],
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
            current_run_id: None,
            cpu_pct: None,
            mem_bytes: None,
            mem_current_bytes: None,
            uptime_s: None,
        };

        // Two idle ticks: since_ts pins to the FIRST one (no edge on the second).
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

        // A liveness change moves since_ts to the change time; last_seen advances.
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
