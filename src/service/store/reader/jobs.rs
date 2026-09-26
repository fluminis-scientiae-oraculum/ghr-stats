//! Hook job-event reads and the tailer's offsets. A job row stays incomplete while it runs,
//! so ordering uses `COALESCE(started_at, completed_at)`.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::shared::error::Result;
use crate::shared::models::{JobRow, PendingConclusion};

/// Keyed by stream (the per-runner event-log path); an absent stream tails from 0.
pub fn ingest_offsets(conn: &Connection) -> Result<HashMap<String, u64>> {
    let mut stmt = conn.prepare_cached("SELECT stream, offset FROM ingest_offset")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

const JOB_ROW: &str = "SELECT runner_name, repo, job, started_at, completed_at, conclusion \
                       FROM job_event";

fn job_row(r: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        runner_name: r.get(0)?,
        repo: r.get(1)?,
        job: r.get(2)?,
        started_at: r.get(3)?,
        completed_at: r.get(4)?,
        conclusion: r.get(5)?,
    })
}

pub fn recent_jobs(conn: &Connection, limit: usize) -> Result<Vec<JobRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "{JOB_ROW} ORDER BY COALESCE(started_at, completed_at) DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map(params![limit as i64], job_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn jobs_awaiting_conclusion(
    conn: &Connection,
    completed_since: i64,
    limit: usize,
) -> Result<Vec<PendingConclusion>> {
    let mut stmt = conn.prepare_cached(
        "SELECT org, repo, run_id, run_attempt, job, runner_name FROM job_event \
         WHERE completed_at >= ?1 AND conclusion IS NULL \
               AND org <> '' AND repo <> '' \
         ORDER BY completed_at ASC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![completed_since, limit as i64], |r| {
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

/// `(total, in-flight)`.
pub fn job_counts(conn: &Connection) -> Result<(i64, i64)> {
    conn.query_row(
        "SELECT count(*), COALESCE(SUM(completed_at IS NULL), 0) FROM job_event",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(Into::into)
}

/// Running or last completed, so an idle runner still shows its last job.
pub fn latest_job(conn: &Connection, runner_name: &str) -> Result<Option<JobRow>> {
    conn.query_row(
        &format!(
            "{JOB_ROW} WHERE runner_name = ?1 \
             ORDER BY COALESCE(started_at, completed_at) DESC LIMIT 1"
        ),
        params![runner_name],
        job_row,
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
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,repo,org,runner_name,started_at,completed_at) \
             VALUES (1,1,'build','o/x','o','r',10,20)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,repo,org,runner_name,started_at,completed_at,conclusion) \
             VALUES (2,1,'t','o/x','o','r',10,20,'success')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,repo,org,runner_name,started_at) \
             VALUES (3,1,'run','o/x','o','r',10)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,runner_name,started_at,completed_at) \
             VALUES (4,1,'x','r',10,20)",
            [],
        )
        .unwrap();

        let p = jobs_awaiting_conclusion(&conn, 0, 10).unwrap();
        assert_eq!(p.len(), 1);
        assert!(jobs_awaiting_conclusion(&conn, 21, 10).unwrap().is_empty());
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
        conn.execute(
            "INSERT INTO job_event (run_id,job,repo,runner_name,started_at,completed_at) \
             VALUES (1,'a','o/x','r',100,150)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO job_event (run_id,job,repo,runner_name,started_at) \
             VALUES (2,'b','o/y','r',200)",
            [],
        )
        .unwrap();
        let j = latest_job(&conn, "r").unwrap().unwrap();
        assert_eq!((j.job.as_str(), j.repo.as_str()), ("b", "o/y"));
        assert!(j.completed_at.is_none()); // running

        conn.execute("UPDATE job_event SET completed_at=260 WHERE run_id=2", [])
            .unwrap();
        let j = latest_job(&conn, "r").unwrap().unwrap();
        assert_eq!((j.job.as_str(), j.completed_at), ("b", Some(260)));

        assert!(latest_job(&conn, "nobody").unwrap().is_none());
    }
}
