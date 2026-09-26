//! Hook job events, the tailer's byte offset, and the conclusion backfill. A job's
//! `started` and `completed` arrive separately and merge into one row.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::hooks::ingest::HookEvent;
use crate::shared::models::JobConclusion;

/// Events and `stream`'s offset commit together, so a failed batch is re-read, not lost.
pub fn apply_hook_events(
    conn: &mut Connection,
    stream: &str,
    runner: &str,
    events: &[HookEvent],
    offset: u64,
) -> Result<()> {
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
            let j = e.job();
            let (started, completed) = match e {
                HookEvent::Started(_) => (Some(j.ts), None),
                HookEvent::Completed(_) => (None, Some(j.ts)),
            };
            let org = j.repo.split_once('/').map_or("", |(o, _)| o);
            stmt.execute(params![
                j.run_id,
                j.run_attempt,
                j.job,
                j.repo,
                org,
                runner,
                started,
                completed,
            ])?;
        }
    }
    tx.execute(
        "INSERT INTO ingest_offset (stream, offset) VALUES (?1, ?2) \
         ON CONFLICT(stream) DO UPDATE SET offset = excluded.offset",
        params![stream, offset as i64],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn apply_job_conclusions(conn: &mut Connection, updates: &[JobConclusion]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "UPDATE job_event SET conclusion = ?5 \
             WHERE run_id = ?1 AND run_attempt = ?2 AND job = ?3 AND runner_name = ?4",
        )?;
        for u in updates {
            stmt.execute(params![
                u.run_id,
                u.run_attempt,
                u.job,
                u.runner_name,
                u.conclusion,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn hook_events_merge_started_and_completed() {
        use crate::shared::hooks::ingest::JobRef;
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        let job = |ts| JobRef {
            ts,
            repo: "example-org/foo".into(),
            run_id: 7,
            run_attempt: 1,
            job: "build".into(),
        };

        apply_hook_events(&mut conn, "log", "r0", &[HookEvent::Started(job(1000))], 10).unwrap();
        apply_hook_events(
            &mut conn,
            "log",
            "r0",
            &[HookEvent::Completed(job(1050))],
            20,
        )
        .unwrap();

        let row: (i64, i64, String, String) = conn
            .query_row(
                "SELECT started_at, completed_at, org, runner_name FROM job_event WHERE run_id=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (1000, 1050, "example-org".into(), "r0".into()));
        let off: i64 = conn
            .query_row(
                "SELECT offset FROM ingest_offset WHERE stream='log'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(off, 20);
    }

    #[test]
    fn per_runner_logs_tail_independently_into_recent_jobs() {
        use crate::service::store::reader;
        use crate::shared::hooks::{ingest, runner_event_log};

        let dir = tempfile::tempdir().unwrap();
        let r1 = dir.path().join("runner-01");
        let r2 = dir.path().join("runner-02");
        std::fs::create_dir_all(&r1).unwrap();
        std::fs::create_dir_all(&r2).unwrap();
        std::fs::write(
            runner_event_log(&r1),
            "{\"phase\":\"started\",\"ts\":1000,\"repo\":\"example-org/foo\",\"run_id\":1,\"job\":\"build\"}\n\
             {\"phase\":\"completed\",\"ts\":1090,\"repo\":\"example-org/foo\",\"run_id\":1,\"job\":\"build\"}\n",
        )
        .unwrap();
        std::fs::write(
            runner_event_log(&r2),
            "{\"phase\":\"started\",\"ts\":1100,\"repo\":\"example-org/bar\",\"run_id\":2,\"job\":\"test\"}\n",
        )
        .unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        let tail_all = |conn: &mut Connection| {
            let offsets = reader::ingest_offsets(conn).unwrap();
            for (dir, name) in [(&r1, "runner-01"), (&r2, "runner-02")] {
                let stream = runner_event_log(dir).to_string_lossy().into_owned();
                let start = offsets.get(&stream).copied().unwrap_or(0);
                let (events, off) = ingest::tail_events(dir, "example-org", start);
                apply_hook_events(conn, &stream, name, &events, off).unwrap();
            }
        };
        tail_all(&mut conn);
        tail_all(&mut conn);

        let jobs = reader::recent_jobs(&conn, 10).unwrap();
        assert_eq!(jobs.len(), 2);
        let by_runner: std::collections::HashMap<_, _> =
            jobs.iter().map(|j| (j.runner_name.as_str(), j)).collect();
        assert_eq!(by_runner["runner-01"].completed_at, Some(1090));
        assert_eq!(by_runner["runner-02"].completed_at, None);
        assert_eq!(reader::ingest_offsets(&conn).unwrap().len(), 2);
    }

    #[test]
    fn apply_job_conclusions_fills_only_the_matched_row() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        conn.execute(
            "INSERT INTO job_event (run_id,run_attempt,job,runner_name,started_at,completed_at) \
             VALUES (7,1,'build','r0',100,150)",
            [],
        )
        .unwrap();

        apply_job_conclusions(
            &mut conn,
            &[JobConclusion {
                run_id: 7,
                run_attempt: 1,
                job: "build".into(),
                runner_name: "r0".into(),
                conclusion: "success".into(),
            }],
        )
        .unwrap();
        let c: Option<String> = conn
            .query_row("SELECT conclusion FROM job_event WHERE run_id=7", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(c.as_deref(), Some("success"));

        apply_job_conclusions(
            &mut conn,
            &[JobConclusion {
                run_id: 999,
                run_attempt: 1,
                job: "x".into(),
                runner_name: "y".into(),
                conclusion: "failure".into(),
            }],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT count(*) FROM job_event", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
