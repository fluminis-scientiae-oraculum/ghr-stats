//! GitHub reconcile reads. Freshness is adjudicated here once, so consumers get a
//! [`GhView`] and never check an age.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::shared::error::Result;
use crate::shared::models::{ApiReconcileState, ApiRunnerState, ApiState, GhView};

/// Newest reading per runner, not per global tick, keyed `(org, agent_id)` since agentId is unique
/// only within an org. Readings older than `max_age` are [`GhView::Stale`].
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

    #[test]
    fn an_org_missing_from_the_newest_tick_keeps_its_last_reading() {
        let conn = mem_db();
        api_sample(&conn, 100, "org-a", 1, 1, 0);
        api_sample(&conn, 100, "org-b", 1, 1, 0);
        // Tick 200: only org-a answered.
        api_sample(&conn, 200, "org-a", 1, 1, 0);

        let m = latest_api_runners(&conn, 200, 180).unwrap();
        let b = m[&("org-b".to_string(), 1)];
        assert_eq!(b.online(), Some(true));
        assert!(matches!(b, GhView::Fresh { age_s: 100, .. }));
        assert!(matches!(
            m[&("org-a".to_string(), 1)],
            GhView::Fresh { age_s: 0, .. }
        ));
    }

    #[test]
    fn a_reading_older_than_max_age_is_stale_not_live() {
        let conn = mem_db();
        api_sample(&conn, 100, "o", 1, 1, 0);

        let fresh = latest_api_runners(&conn, 160, 180).unwrap();
        assert!(matches!(fresh[&("o".to_string(), 1)], GhView::Fresh { .. }));

        let old = latest_api_runners(&conn, 100 + 21_600, 180).unwrap();
        let v = old[&("o".to_string(), 1)];
        assert!(matches!(v, GhView::Stale { .. }));
        assert_eq!(v.online(), None);
        assert!(matches!(v, GhView::Stale { age_s: 21_600 }));
    }
}
