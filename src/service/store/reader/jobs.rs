//! What the runners' hooks reported — the ingested `job_event` log, and the
//! tailer's own byte offsets into the streams it read them from.
//!
//! Written by the hook tailer rather than by either sampler, which is why these
//! reads sit apart: a job row arrives when a workflow step fires, not on a tick,
//! and it can stay INCOMPLETE for as long as the job runs. [`ingest_offsets`] is
//! here for the same reason — an offset is meaningless except as the tailer's
//! place in the stream that produced these rows, so it moves when they do.
//!
//! That incompleteness shapes every query here. [`recent_jobs`] and
//! [`latest_job`] order by `COALESCE(started_at, completed_at)` so a running job
//! sorts by when it began; [`jobs_awaiting_conclusion`] is the reconcile's
//! work-list of rows the hook could not finish, and requires `org` + `repo`
//! because without both there is no API call to make; and [`latest_job`] is the
//! only reader in the module that may legitimately match nothing, hence the
//! `OptionalExtension` that appears nowhere else.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::shared::error::Result;
use crate::shared::models::{JobRow, PendingConclusion};

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
    use super::super::fixtures::mem_db;
    use super::*;

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
