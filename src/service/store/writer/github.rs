//! What GitHub said — the reconcile thread's writes.
//!
//! One transaction covering four tables, so a scrape can never observe the
//! samples without the edge that explains them. The edge is the reason this is a
//! transaction and not four statements: during the 2026-07-25 incident the raw
//! `online` bit flapped 62 times in three hours, and only `since_ts` — pinned
//! until the value actually flips — turns that into a duration an alert can
//! debounce on.
//!
//! "Only a successful fetch moves the edge" is not enforced here. It is a
//! property of [`ApiOrgOutcome`]: the `Failed` and `Unconfigured` arms carry no
//! rows, so there is nothing to iterate, and a network blip cannot be written as
//! "every runner in this org went offline".

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::models::ApiOrgOutcome;

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
}
