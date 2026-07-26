//! What GitHub said — the reconcile thread's rows.
//!
//! Three tables, one writer: `api_runner_sample` (the per-tick audit trail),
//! `api_runner_state` (the liveness edge an alert debounces on) and
//! `api_reconcile_state` (per-org fetch health). Reading them is a separate job
//! from reading the local sampler's tables, and the 2026-07-25 outage is why:
//! the fleet looked healthy for four hours because the local view was the only
//! one anything consumed, and a GitHub reading that has gone quiet must be
//! visibly aged rather than silently served as current.
//!
//! That adjudication happens HERE, once, in [`latest_api_runners`] — the reader
//! hands out a [`GhView`] that has already decided fresh-vs-stale, so no
//! downstream consumer can forget to check an age.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::shared::error::Result;
use crate::shared::models::{ApiReconcileState, ApiRunnerState, ApiState, GhView};

/// GitHub's latest view of every runner, keyed by `(org, agent_id)`. GitHub's
/// `agent_id` is unique only within an org, so the org must be part of the key —
/// two runners in different orgs can share an id.
///
/// Latest **per runner**, not per global tick. The old query took `max(ts)` over
/// the whole table and returned only rows at that instant, which had two bad
/// consequences: an org that failed a single reconcile had no row at the newest
/// ts, so its runners' GitHub series *vanished* from the export rather than
/// reporting a value; and there was no age bound at all, so if the reconcile
/// thread died the last successful tick was served as current forever.
///
/// `max_age` closes both. Each runner keeps its own most recent reading, and
/// anything older than the window is returned as [`GhView::Stale`] — visibly
/// aged, never silently presented as live.
pub fn latest_api_runners(
    conn: &Connection,
    now: i64,
    max_age: u64,
) -> Result<HashMap<(String, i64), GhView>> {
    let mut stmt = conn.prepare_cached(
        "SELECT s.org, s.agent_id, s.online, s.busy, s.ts \
         FROM api_runner_sample s \
         JOIN (SELECT org, agent_id, max(ts) AS ts \
               FROM api_runner_sample GROUP BY org, agent_id) m \
           ON m.org = s.org AND m.agent_id = s.agent_id AND m.ts = s.ts",
    )?;
    let rows = stmt.query_map([], |r| {
        let ts: i64 = r.get(4)?;
        let state = ApiState {
            online: r.get::<_, i64>(2)? != 0,
            busy: r.get::<_, i64>(3)? != 0,
        };
        let view = GhView::observed(state, now - ts, max_age);
        Ok(((r.get::<_, String>(0)?, r.get::<_, i64>(1)?), view))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The persisted GitHub-side liveness edges, keyed by `(org, agent_id)`. Feeds
/// `ghr_runner_github_offline_seconds` — the duration an alert debounces on.
pub fn api_runner_states(conn: &Connection) -> Result<HashMap<(String, i64), ApiRunnerState>> {
    let mut stmt = conn.prepare_cached(
        "SELECT org, agent_id, online, since_ts, last_seen_ts FROM api_runner_state",
    )?;
    let rows = stmt.query_map([], |r| {
        let org: String = r.get(0)?;
        let agent_id: i64 = r.get(1)?;
        Ok((
            (org.clone(), agent_id),
            ApiRunnerState {
                org,
                agent_id,
                online: r.get::<_, i64>(2)? != 0,
                since_ts: r.get(3)?,
                last_seen_ts: r.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Per-org reconcile health, newest state per org. Empty before the first
/// reconcile tick.
pub fn api_reconcile_states(conn: &Connection) -> Result<Vec<ApiReconcileState>> {
    let mut stmt = conn.prepare_cached(
        "SELECT org, last_ok_ts, last_try_ts, ok, http_status, error_kind, configured \
         FROM api_reconcile_state ORDER BY org",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ApiReconcileState {
            org: r.get(0)?,
            last_ok_ts: r.get(1)?,
            last_try_ts: r.get(2)?,
            ok: r.get::<_, i64>(3)? != 0,
            http_status: r.get::<_, Option<i64>>(4)?.map(|v| v as u16),
            error_kind: r.get(5)?,
            configured: r.get::<_, i64>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{api_sample, mem_db};
    use super::*;

    #[test]
    fn latest_api_runners_takes_the_newest_row_per_runner() {
        let conn = mem_db();
        api_sample(&conn, 100, "o", 1, 1, 0); // older reading for r1
        api_sample(&conn, 200, "o", 1, 1, 1); // newer: r1 now busy
        api_sample(&conn, 200, "o", 2, 0, 0); // r2 offline

        let m = latest_api_runners(&conn, 200, 180).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[&("o".to_string(), 1)].busy(), Some(true));
        assert_eq!(m[&("o".to_string(), 1)].online(), Some(true));
        assert_eq!(m[&("o".to_string(), 2)].online(), Some(false));
        assert!(latest_api_runners(&mem_db(), 200, 180).unwrap().is_empty());
    }

    /// The regression that made the 2026-07-25 outage undiagnosable: with a
    /// global `max(ts)`, an org absent from the newest tick had no row at that
    /// instant, so its runners' GitHub series VANISHED from the export rather
    /// than reporting a value. A missing series and a zero are indistinguishable
    /// to a scrape, which is exactly the wrong answer at 2am.
    #[test]
    fn an_org_missing_from_the_newest_tick_keeps_its_last_reading() {
        let conn = mem_db();
        // Tick 100: both orgs answered.
        api_sample(&conn, 100, "org-a", 1, 1, 0);
        api_sample(&conn, 100, "org-b", 1, 1, 0);
        // Tick 200: only org-a answered (org-b's token broke).
        api_sample(&conn, 200, "org-a", 1, 1, 0);

        let m = latest_api_runners(&conn, 200, 180).unwrap();
        // org-b is still PRESENT, carrying its older reading and an honest age.
        let b = m[&("org-b".to_string(), 1)];
        assert_eq!(b.online(), Some(true));
        assert!(matches!(b, GhView::Fresh { age_s: 100, .. }));
        // org-a is current.
        assert!(matches!(
            m[&("org-a".to_string(), 1)],
            GhView::Fresh { age_s: 0, .. }
        ));
    }

    /// Past the window a reading is reported as stale, never served as current.
    /// Without this a dead reconcile thread kept exporting confident values
    /// forever.
    #[test]
    fn a_reading_older_than_max_age_is_stale_not_live() {
        let conn = mem_db();
        api_sample(&conn, 100, "o", 1, 1, 0);

        // 60s later, inside a 180s window: still trustworthy.
        let fresh = latest_api_runners(&conn, 160, 180).unwrap();
        assert!(matches!(fresh[&("o".to_string(), 1)], GhView::Fresh { .. }));

        // 6 hours later: we still have the row, but it is not evidence of
        // anything current — online() must go unknown rather than stay `true`.
        let old = latest_api_runners(&conn, 100 + 21_600, 180).unwrap();
        let v = old[&("o".to_string(), 1)];
        assert!(matches!(v, GhView::Stale { .. }));
        assert_eq!(v.online(), None);
        assert!(matches!(v, GhView::Stale { age_s: 21_600 }));
    }
}
