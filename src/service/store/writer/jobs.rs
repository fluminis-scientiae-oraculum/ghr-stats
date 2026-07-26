//! What the runners' hooks reported — the `job_event` log, the tailer's byte
//! offset into it, and the conclusion that arrives later.
//!
//! A job row is not written on a tick. It arrives when a workflow step fires, and
//! it can stay INCOMPLETE for as long as the job runs, which is why both writes
//! here are merges rather than inserts: `started` and `completed` for one job key
//! `COALESCE` into a single row, each filling only the timestamp it carries.
//!
//! [`apply_job_conclusions`] sits here despite being called by the reconcile
//! thread, because the rows are the hooks' — it fills the one column the hook
//! could not, and touches nothing the hook owns. Its read half,
//! `reader::jobs_awaiting_conclusion`, sits in the matching child on the read
//! side for the same reason.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::hooks::ingest::HookEvent;
use crate::shared::models::JobConclusion;

/// Upsert hook job events and advance the ingest offset for `stream`, atomically.
/// `started` and `completed` for the same job key merge into one row (each fills
/// the timestamp it carries without clobbering the other). `stream` is the
/// tailed-log identifier (the per-runner event-log path) so each runner's log
/// tracks its own byte offset independently.
pub fn apply_hook_events(
    conn: &mut Connection,
    stream: &str,
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
        "INSERT INTO ingest_offset (stream, offset) VALUES (?1, ?2) \
         ON CONFLICT(stream) DO UPDATE SET offset = excluded.offset",
        params![stream, offset as i64],
    )?;
    tx.commit()?;
    Ok(())
}

/// Write resolved job conclusions back to `job_event` (the API reconcile pass).
/// One transaction; a row that no longer matches (pruned/renamed) is a harmless
/// no-op. Only fills the conclusion — never touches the hook-owned timing.
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
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
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

        apply_hook_events(&mut conn, "hooks", &[started], 10).unwrap();
        apply_hook_events(&mut conn, "hooks", &[completed], 20).unwrap();

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

    /// End-to-end regression for the "jobs picked up but never shown" bug: each
    /// runner writes its OWN event log (a file it owns), the collector tails each
    /// with an INDEPENDENT per-stream offset, and both runners' jobs land in the
    /// Jobs view. This is the multi-runner shape the old single-shared-log design
    /// silently dropped when the shared path wasn't writable.
    #[test]
    fn per_runner_logs_tail_independently_into_recent_jobs() {
        use crate::service::store::reader;
        use crate::shared::hooks::{ingest, runner_event_log};

        let dir = tempfile::tempdir().unwrap();
        // Two runners, each with its own install dir + own event log.
        let r1 = dir.path().join("runner-01");
        let r2 = dir.path().join("runner-02");
        std::fs::create_dir_all(&r1).unwrap();
        std::fs::create_dir_all(&r2).unwrap();
        let log1 = runner_event_log(&r1);
        let log2 = runner_event_log(&r2);

        // runner-01 completes a job; runner-02 starts one (still running).
        std::fs::write(
            &log1,
            "{\"phase\":\"started\",\"ts\":1000,\"repo\":\"example-org/foo\",\"run_id\":1,\"job\":\"build\",\"runner\":\"runner-01\"}\n\
             {\"phase\":\"completed\",\"ts\":1090,\"repo\":\"example-org/foo\",\"run_id\":1,\"job\":\"build\",\"runner\":\"runner-01\"}\n",
        )
        .unwrap();
        std::fs::write(
            &log2,
            "{\"phase\":\"started\",\"ts\":1100,\"repo\":\"example-org/bar\",\"run_id\":2,\"job\":\"test\",\"runner\":\"runner-02\"}\n",
        )
        .unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);

        // Collector tail loop: one (stream, offset) per runner log.
        let mut offsets: std::collections::HashMap<String, u64> =
            reader::ingest_offsets(&conn).unwrap();
        for log in [&log1, &log2] {
            let stream = log.to_string_lossy().into_owned();
            let start = offsets.get(&stream).copied().unwrap_or(0);
            let (events, new_off) = ingest::tail_events(log, start);
            apply_hook_events(&mut conn, &stream, &events, new_off).unwrap();
            offsets.insert(stream, new_off);
        }

        // Both runners' jobs are present.
        let jobs = reader::recent_jobs(&conn, 10).unwrap();
        assert_eq!(jobs.len(), 2);
        let by_runner: std::collections::HashMap<_, _> =
            jobs.iter().map(|j| (j.runner_name.as_str(), j)).collect();
        assert_eq!(by_runner["runner-01"].completed_at, Some(1090));
        assert_eq!(by_runner["runner-02"].completed_at, None); // still running

        // Offsets advanced per stream, independently, and are persisted.
        let persisted = reader::ingest_offsets(&conn).unwrap();
        assert_eq!(persisted.len(), 2);
        assert_eq!(
            persisted[&log1.to_string_lossy().into_owned()],
            offsets[&log1.to_string_lossy().into_owned()]
        );
        assert!(persisted[&log1.to_string_lossy().into_owned()] > 0);

        // A second tail from the advanced offsets yields nothing new (idempotent).
        let stream1 = log1.to_string_lossy().into_owned();
        let (events, off) = ingest::tail_events(&log1, offsets[&stream1]);
        assert!(events.is_empty());
        assert_eq!(off, offsets[&stream1]);
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

        // A non-matching update is a harmless no-op — no panic, no new row.
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
