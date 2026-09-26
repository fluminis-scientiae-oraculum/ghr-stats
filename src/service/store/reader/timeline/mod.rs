//! `timeline` readers. Edges are derived in SQL via `LAG`, so the limit bounds the
//! collector's work, not only the reply. An edge needs its predecessor inside the window
//! (the first sample is a baseline), so an incremental consumer such as `ops::tail` must
//! overlap its windows.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::models::timeline::{Bounded, Timeline, TimelinePoint, TimelineQuery, Window};
use crate::shared::models::{ApiState, GhView};
use crate::shared::util::to_rfc3339_utc;

mod edges;

use edges::{github_edges, job_edges, liveness_edges, reconcile_edges};

pub fn timeline(conn: &Connection, q: &TimelineQuery, now: i64, max_age: u64) -> Result<Timeline> {
    // One row over the limit per stream shows the union was cut.
    let probe = q.limit.saturating_add(1);
    let mut rows = liveness_edges(conn, q, probe)?;
    rows.extend(github_edges(conn, q, probe)?);
    rows.extend(reconcile_edges(conn, q, probe)?);
    // Stable sort: same-timestamp edges keep a deterministic order.
    rows.sort_by_key(|t| std::cmp::Reverse(t.ts));

    let samples = q
        .samples
        .then(|| timeline_samples(conn, q, probe, max_age))
        .transpose()?
        .map(|rows| Bounded::newest(rows, q.limit));

    Ok(Timeline {
        schema_version: 1,
        generated_at: to_rfc3339_utc(now),
        generated_at_epoch: now,
        window: Window {
            since: to_rfc3339_utc(q.since_ts),
            since_epoch: q.since_ts,
            until_epoch: now,
            truncated_at: earliest_sample(conn, q)?.filter(|first| *first > q.since_ts),
        },
        transitions: Bounded::newest(rows, q.limit),
        jobs: Bounded::newest(job_edges(conn, q, probe)?, q.limit),
        samples,
    })
}

/// Bound as `(?n IS NULL OR col = ?n)`, so one statement shape keeps `prepare_cached` effective.
pub(super) fn filters(q: &TimelineQuery) -> (Option<&str>, Option<&str>) {
    (q.org.as_deref(), q.runner.as_deref())
}

/// GitHub's view as of each tick: the newest reading at or before it, never after.
fn timeline_samples(
    conn: &Connection,
    q: &TimelineQuery,
    limit: usize,
    max_age: u64,
) -> Result<Vec<TimelinePoint>> {
    let (org, runner) = filters(q);
    let mut stmt = conn.prepare_cached(
        "SELECT r.ts, r.org, r.name, r.liveness, a.ts, a.online, a.busy \
         FROM runner_sample r \
         LEFT JOIN api_runner_sample a \
                ON a.org = r.org AND a.agent_id = r.agent_id \
               AND a.ts = (SELECT max(x.ts) FROM api_runner_sample x \
                            WHERE x.org = r.org AND x.agent_id = r.agent_id AND x.ts <= r.ts) \
         WHERE r.ts >= ?1 AND (?2 IS NULL OR r.org = ?2) AND (?3 IS NULL OR r.name = ?3) \
         ORDER BY r.ts DESC, r.name ASC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![q.since_ts, org, runner, limit as i64], |r| {
        let ts: i64 = r.get(0)?;
        let github = match r.get::<_, Option<i64>>(4)? {
            Some(gh_ts) => GhView::observed(
                ApiState {
                    online: r.get::<_, i64>(5)? != 0,
                    busy: r.get::<_, i64>(6)? != 0,
                },
                ts - gh_ts,
                max_age,
            ),
            None => GhView::Unknown,
        };
        Ok(TimelinePoint {
            ts,
            org: r.get(1)?,
            runner: r.get(2)?,
            liveness: r.get(3)?,
            github,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn earliest_sample(conn: &Connection, q: &TimelineQuery) -> Result<Option<i64>> {
    let (org, runner) = filters(q);
    conn.query_row(
        "SELECT min(ts) FROM runner_sample \
         WHERE (?1 IS NULL OR org = ?1) AND (?2 IS NULL OR name = ?2)",
        params![org, runner],
        |r| r.get(0),
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::Liveness;
    use crate::shared::models::timeline::{Edge, ReconcileEdge};

    fn mem_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        conn
    }

    fn local_sample(conn: &Connection, ts: i64, org: &str, id: i64, name: &str, live: &str) {
        conn.execute(
            "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ts, id, name, org, live, format!("/srv/{org}/{name}")],
        )
        .unwrap();
    }

    fn api_named(conn: &Connection, ts: i64, org: &str, id: i64, name: &str, online: i64) {
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![ts, id, org, name, online],
        )
        .unwrap();
    }

    fn reconcile_sample(conn: &Connection, ts: i64, org: &str, ok: i64, kind: Option<&str>) {
        conn.execute(
            "INSERT INTO api_reconcile_sample (ts, org, ok, error_kind, http_status, runners) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![ts, org, ok, kind, kind.map(|_| 401)],
        )
        .unwrap();
    }

    fn tq(since_ts: i64, limit: usize) -> TimelineQuery {
        TimelineQuery {
            since_ts,
            limit,
            org: None,
            runner: None,
            samples: false,
        }
    }

    #[test]
    fn timeline_returns_edges_not_samples() {
        let conn = mem_db();
        for (ts, live) in [
            (100, "idle"),
            (200, "idle"),
            (300, "busy"),
            (400, "busy"),
            (500, "idle"),
        ] {
            local_sample(&conn, ts, "o", 1, "r1", live);
        }
        let t = timeline(&conn, &tq(0, 100), 600, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 2);
        assert!(!t.transitions.limited);
        assert!(matches!(
            &t.transitions.items[0].edge,
            Edge::Liveness { runner, from: Liveness::Idle, to: Liveness::Busy } if runner == "r1"
        ));
        assert_eq!(t.transitions.items[0].ts, 300);
        assert!(matches!(
            &t.transitions.items[1].edge,
            Edge::Liveness {
                from: Liveness::Busy,
                to: Liveness::Idle,
                ..
            }
        ));
        assert_eq!(t.transitions.items[1].ts, 500);
    }

    #[test]
    fn the_first_sample_in_a_window_is_a_baseline_not_an_edge() {
        let conn = mem_db();
        local_sample(&conn, 100, "o", 1, "r1", "idle");
        local_sample(&conn, 200, "o", 1, "r1", "busy");
        let t = timeline(&conn, &tq(200, 100), 300, 180).unwrap();
        assert!(t.transitions.items.is_empty());
        let t = timeline(&conn, &tq(100, 100), 300, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 1);
    }

    #[test]
    fn github_edges_come_from_consecutive_readings_not_the_local_tick_grid() {
        let conn = mem_db();
        api_named(&conn, 100, "o", 1, "r1", 1);
        api_named(&conn, 400, "o", 1, "r1", 0);
        api_named(&conn, 500, "o", 1, "r1", 0);

        let t = timeline(&conn, &tq(0, 100), 600, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 1);
        assert_eq!(t.transitions.items[0].ts, 400);
        assert!(matches!(
            &t.transitions.items[0].edge,
            Edge::GithubOnline {
                runner,
                online: false
            } if runner == "r1"
        ));
    }

    #[test]
    fn reconcile_edges_carry_the_cause_and_the_recovery() {
        let conn = mem_db();
        reconcile_sample(&conn, 100, "org-b", 1, None);
        reconcile_sample(&conn, 200, "org-b", 0, Some("unauthorized"));
        reconcile_sample(&conn, 300, "org-b", 0, Some("unauthorized"));
        reconcile_sample(&conn, 400, "org-b", 1, None);

        let t = timeline(&conn, &tq(0, 100), 500, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 2);
        assert!(matches!(
            &t.transitions.items[0].edge,
            Edge::Reconcile(ReconcileEdge::Failed { error_kind, http_status })
                if error_kind.as_deref() == Some("unauthorized") && *http_status == Some(401)
        ));
        assert!(matches!(
            &t.transitions.items[1].edge,
            Edge::Reconcile(ReconcileEdge::Recovered)
        ));
    }

    #[test]
    fn a_runner_filter_keeps_its_own_orgs_reconcile_edges() {
        let conn = mem_db();
        local_sample(&conn, 100, "org-a", 1, "r1", "idle");
        local_sample(&conn, 200, "org-a", 1, "r1", "busy");
        reconcile_sample(&conn, 100, "org-a", 1, None);
        reconcile_sample(&conn, 200, "org-a", 0, Some("server_error"));
        reconcile_sample(&conn, 100, "org-z", 1, None);
        reconcile_sample(&conn, 200, "org-z", 0, Some("unauthorized"));

        let mut q = tq(0, 100);
        q.runner = Some("r1".to_string());
        let t = timeline(&conn, &q, 300, 180).unwrap();

        assert_eq!(t.transitions.items.len(), 2);
        assert!(t.transitions.items.iter().all(|tr| tr.org == "org-a"));
        assert!(
            t.transitions
                .items
                .iter()
                .any(|tr| matches!(tr.edge, Edge::Reconcile(_)))
        );
    }

    #[test]
    fn samples_never_borrow_a_reading_from_the_future() {
        let conn = mem_db();
        local_sample(&conn, 800, "o", 1, "r1", "idle"); // before any reading
        local_sample(&conn, 1000, "o", 1, "r1", "idle");
        local_sample(&conn, 1200, "o", 1, "r1", "idle");
        api_named(&conn, 900, "o", 1, "r1", 1);
        api_named(&conn, 1300, "o", 1, "r1", 0); // AFTER the last local tick

        let mut q = tq(0, 100);
        q.samples = true;
        let t = timeline(&conn, &q, 1400, 180).unwrap();
        let s = t.samples.unwrap();
        assert_eq!(s.items.len(), 3);

        assert!(matches!(s.items[0].github, GhView::Unknown));
        assert!(matches!(
            s.items[1].github,
            GhView::Fresh { age_s: 100, .. }
        ));
        assert!(matches!(s.items[2].github, GhView::Stale { age_s: 300 }));
    }

    #[test]
    fn a_window_reaching_past_retention_reports_where_the_data_starts() {
        let conn = mem_db();
        local_sample(&conn, 5_000, "o", 1, "r1", "idle");
        local_sample(&conn, 5_100, "o", 1, "r1", "busy");

        let t = timeline(&conn, &tq(0, 100), 6_000, 180).unwrap();
        assert_eq!(t.window.truncated_at, Some(5_000));
        let t = timeline(&conn, &tq(5_000, 100), 6_000, 180).unwrap();
        assert_eq!(t.window.truncated_at, None);
    }

    #[test]
    fn the_limit_bounds_the_union_and_reports_the_cut() {
        let conn = mem_db();
        // Alternating liveness: one edge per sample after the first.
        for (i, ts) in (100..=600).step_by(100).enumerate() {
            let live = if i.is_multiple_of(2) { "idle" } else { "busy" };
            local_sample(&conn, ts, "o", 1, "r1", live);
        }
        reconcile_sample(&conn, 100, "o", 1, None);
        reconcile_sample(&conn, 550, "o", 0, Some("timeout"));

        let t = timeline(&conn, &tq(0, 3), 700, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 3);
        assert!(t.transitions.limited);
        assert_eq!(
            t.transitions.items.iter().map(|x| x.ts).collect::<Vec<_>>(),
            vec![500, 550, 600]
        );

        let t = timeline(&conn, &tq(0, 100), 700, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 6);
        assert!(!t.transitions.limited);
    }

    #[test]
    fn samples_are_absent_unless_requested() {
        let conn = mem_db();
        local_sample(&conn, 100, "o", 1, "r1", "idle");

        assert!(
            timeline(&conn, &tq(0, 100), 200, 180)
                .unwrap()
                .samples
                .is_none()
        );

        let mut q = tq(0, 100);
        q.samples = true;
        assert_eq!(
            timeline(&conn, &q, 200, 180)
                .unwrap()
                .samples
                .unwrap()
                .items
                .len(),
            1
        );

        q.since_ts = 1_000;
        let s = timeline(&conn, &q, 2_000, 180).unwrap().samples.unwrap();
        assert!(s.items.is_empty());
    }

    #[test]
    fn edges_partition_by_org_and_id_not_id_alone() {
        let conn = mem_db();
        for ts in [100, 200, 300] {
            local_sample(&conn, ts, "org-a", 22, "a-runner", "idle");
            local_sample(&conn, ts, "org-b", 22, "b-runner", "busy");
            api_named(&conn, ts, "org-a", 22, "a-runner", 1);
            api_named(&conn, ts, "org-b", 22, "b-runner", 0);
        }
        let t = timeline(&conn, &tq(0, 100), 400, 180).unwrap();
        assert!(
            t.transitions.items.is_empty(),
            "neither runner changed: {:?}",
            t.transitions.items
        );
    }
}
