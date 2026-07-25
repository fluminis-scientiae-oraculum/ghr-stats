//! Collector-side writes. One transaction per tick keeps a sample atomic.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::hooks::ingest::HookEvent;
use crate::shared::models::{ApiOrgOutcome, HostSample, JobConclusion, RunnerSample};

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

/// Persist one GitHub API reconcile tick (all orgs share a single `ts`).
///
/// Four writes, one transaction, so a scrape can never observe the samples
/// without the edge that explains them:
///
/// 1. `api_runner_sample` — the raw per-tick observation (unchanged).
/// 2. `api_runner_state` — the online/offline EDGE, mirroring the local
///    `runner_state` upsert above. `since_ts` only moves when `online` actually
///    flips, which is what turns a flapping bit into an alertable duration.
/// 3. `api_reconcile_state` — current per-org health, so "we could not ask" is
///    distinguishable from "GitHub said offline".
/// 4. `api_reconcile_sample` — the per-tick audit trail.
///
/// Only a successful fetch moves the edge. That is not a rule enforced here but
/// a property of [`ApiOrgOutcome`]: the `Failed` and `Unconfigured` arms carry
/// no rows, so there is nothing to iterate. A network blip cannot be recorded
/// as "every runner in this org went offline".
pub fn write_api_runners(conn: &mut Connection, ts: i64, outcomes: &[ApiOrgOutcome]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut sample = tx.prepare_cached(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut edge = tx.prepare_cached(
            "INSERT INTO api_runner_state (org, agent_id, online, since_ts, last_seen_ts) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT(org, agent_id) DO UPDATE SET \
                 since_ts = CASE WHEN api_runner_state.online = excluded.online \
                                 THEN api_runner_state.since_ts ELSE excluded.since_ts END, \
                 online = excluded.online, \
                 last_seen_ts = excluded.last_seen_ts",
        )?;
        // `last_ok_ts` uses COALESCE on the EXCLUDED value so a failing tick
        // leaves the last success timestamp intact — that gap is precisely what
        // `ghr_api_reconcile_timestamp_seconds` must expose.
        let mut health = tx.prepare_cached(
            "INSERT INTO api_reconcile_state \
                 (org, last_ok_ts, last_try_ts, ok, http_status, error_kind, configured) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(org) DO UPDATE SET \
                 last_ok_ts = COALESCE(excluded.last_ok_ts, api_reconcile_state.last_ok_ts), \
                 last_try_ts = excluded.last_try_ts, \
                 ok = excluded.ok, \
                 http_status = excluded.http_status, \
                 error_kind = excluded.error_kind, \
                 configured = excluded.configured",
        )?;
        let mut audit = tx.prepare_cached(
            "INSERT INTO api_reconcile_sample (ts, org, ok, http_status, error_kind, runners) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        for outcome in outcomes {
            let org = outcome.org();
            let (ok, kind, runners) = match outcome {
                ApiOrgOutcome::Ok { rows, .. } => {
                    for r in rows {
                        sample.execute(params![
                            ts,
                            r.agent_id,
                            r.org,
                            r.name,
                            r.online as i64,
                            r.busy as i64
                        ])?;
                        edge.execute(params![r.org, r.agent_id, r.online as i64, ts])?;
                    }
                    (true, None, rows.len())
                }
                ApiOrgOutcome::Failed { kind, .. } => (false, Some(*kind), 0),
                ApiOrgOutcome::Unconfigured { .. } => (false, None, 0),
            };
            let configured = !matches!(outcome, ApiOrgOutcome::Unconfigured { .. });
            let error_kind = kind.map(|k| k.label());
            let http_status = kind.and_then(|k| k.http_status());
            let last_ok_ts = ok.then_some(ts);

            health.execute(params![
                org,
                last_ok_ts,
                ts,
                ok as i64,
                http_status,
                error_kind,
                configured as i64,
            ])?;
            audit.execute(params![
                ts,
                org,
                ok as i64,
                http_status,
                error_kind,
                runners as i64
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

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
    use crate::shared::models::{ApiErrorKind, ApiRunnerRow};
    use rusqlite::Connection;

    fn api_row(org: &str, agent_id: i64, online: bool) -> ApiRunnerRow {
        ApiRunnerRow {
            agent_id,
            org: org.to_string(),
            name: format!("runner-{agent_id}"),
            online,
            busy: false,
        }
    }

    /// One reconcile tick.
    fn tick(conn: &mut Connection, ts: i64, outcomes: Vec<ApiOrgOutcome>) {
        write_api_runners(conn, ts, &outcomes).unwrap();
    }

    /// A successful fetch of one org reporting a single runner.
    fn ok_org(org: &str, agent_id: i64, online: bool) -> Vec<ApiOrgOutcome> {
        vec![ApiOrgOutcome::Ok {
            org: org.to_string(),
            rows: vec![api_row(org, agent_id, online)],
        }]
    }

    fn edge(conn: &Connection, org: &str, agent_id: i64) -> (i64, i64, i64) {
        conn.query_row(
            "SELECT online, since_ts, last_seen_ts FROM api_runner_state \
             WHERE org = ?1 AND agent_id = ?2",
            params![org, agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    /// The edge only moves when `online` actually flips. This is what makes
    /// "offline for >15m" alertable: during the 2026-07-25 incident the raw bit
    /// flapped 62 times in three hours, so anything derived from the
    /// instantaneous value would have fired and been muted long before the
    /// sustained failure arrived.
    #[test]
    fn api_edge_since_ts_holds_while_online_is_unchanged_and_moves_on_a_flip() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        tick(&mut conn, 100, ok_org("org-a", 1, true));
        assert_eq!(edge(&conn, "org-a", 1), (1, 100, 100));

        // Same state a tick later: since_ts pinned, last_seen_ts advances.
        tick(&mut conn, 200, ok_org("org-a", 1, true));
        assert_eq!(edge(&conn, "org-a", 1), (1, 100, 200));

        // Flip to offline: since_ts jumps to the tick of the change.
        tick(&mut conn, 300, ok_org("org-a", 1, false));
        assert_eq!(edge(&conn, "org-a", 1), (0, 300, 300));

        // Still offline: pinned again, so `now - since_ts` grows monotonically
        // across the flap instead of resetting on every tick.
        tick(&mut conn, 900, ok_org("org-a", 1, false));
        assert_eq!(edge(&conn, "org-a", 1), (0, 300, 900));
    }

    /// The edge lives in the DB, not in collector memory, so a restart does not
    /// reset the duration an alert is debouncing on.
    #[test]
    fn api_edge_survives_a_collector_restart() {
        // A real restart: an on-disk DB, closed and reopened on a NEW
        // connection. An in-memory DB cannot express this — it dies with the
        // connection, and re-running the (idempotent) migration on the same
        // handle would prove nothing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ghr-stats.db");

        {
            let mut conn = Connection::open(&path).unwrap();
            crate::service::store::schema_for_test(&mut conn);
            tick(&mut conn, 100, ok_org("org-a", 1, false));
        } // dropped == collector stopped

        let mut conn = Connection::open(&path).unwrap();
        crate::service::store::schema_for_test(&mut conn);
        tick(&mut conn, 700, ok_org("org-a", 1, false));

        // since_ts is still the ORIGINAL edge, so the duration a ">15m offline"
        // alert debounces on is not reset by a service restart or an upgrade.
        assert_eq!(edge(&conn, "org-a", 1), (0, 100, 700));
    }

    /// A failed fetch must never be recorded as "the runners went offline", and
    /// must not silently erase the org either. The enum makes the first half
    /// unrepresentable; this pins the second: health and audit rows still land.
    #[test]
    fn a_failed_org_records_health_without_touching_the_edge() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);

        tick(&mut conn, 100, ok_org("org-a", 1, true));

        // Next tick the token breaks.
        tick(
            &mut conn,
            200,
            vec![ApiOrgOutcome::Failed {
                org: "org-a".into(),
                kind: ApiErrorKind::Forbidden,
            }],
        );

        // The edge is untouched — still online since 100, NOT flipped offline.
        assert_eq!(edge(&conn, "org-a", 1), (1, 100, 100));

        let (last_ok, last_try, ok, status, kind): (i64, i64, i64, i64, String) = conn
            .query_row(
                "SELECT last_ok_ts, last_try_ts, ok, http_status, error_kind \
                 FROM api_reconcile_state WHERE org='org-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        // The last SUCCESS timestamp survives the failure — that gap is exactly
        // what `ghr_api_reconcile_timestamp_seconds` exposes.
        assert_eq!((last_ok, last_try, ok), (100, 200, 0));
        assert_eq!((status, kind.as_str()), (403, "http_403"));
    }

    /// Regression: a tick where EVERY org fails used to leave no trace at all,
    /// because the producer only sent a sample when it had rows. A fleet-wide
    /// outage is when the record matters most.
    #[test]
    fn a_tick_where_every_org_fails_still_writes_health_and_audit_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);

        tick(
            &mut conn,
            500,
            vec![
                ApiOrgOutcome::Failed {
                    org: "org-a".into(),
                    kind: ApiErrorKind::Transport,
                },
                ApiOrgOutcome::Unconfigured {
                    org: "org-b".into(),
                },
            ],
        );

        let audit: i64 = conn
            .query_row("SELECT count(*) FROM api_reconcile_sample", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(audit, 2);

        // "Never reached GitHub" has no HTTP status; "no PAT configured" is
        // neither an error nor absent — it reports configured = 0.
        let (status, kind): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT http_status, error_kind FROM api_reconcile_state WHERE org='org-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, None);
        assert_eq!(kind.as_deref(), Some("transport"));

        let (configured, kind_b): (i64, Option<String>) = conn
            .query_row(
                "SELECT configured, error_kind FROM api_reconcile_state WHERE org='org-b'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((configured, kind_b), (0, None));
    }

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
