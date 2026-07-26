//! The four edge streams: what CHANGED between consecutive samples.
//!
//! Four queries, one argument. Each is a `LAG` over its own partition, so the
//! `LIMIT` bounds the collector's work rather than only the reply — a six-hour
//! window on this fleet is ~90 000 local samples, and a limit applied after
//! materialising them would bound neither.
//!
//! They live together because they share the invariant, not because they share a
//! producer: an edge exists only where BOTH a row and its predecessor fall inside
//! the window, so a change to how an edge is bounded lands on all four at once.
//! The parent owns the window that bounds them.
//!
//! Each stream keeps its own reading of "a change", and the differences are
//! deliberate. A GitHub edge means GitHub told us something different from the
//! last time it told us anything — so a reconcile GAP produces no edge, because
//! we did not observe a change, we stopped observing. `--runner` narrows
//! reconcile edges to that runner's ORGS rather than filtering them out, since
//! "could we reach GitHub for this runner's org" is the context that makes the
//! runner's own edges readable.

use rusqlite::{Connection, params};

use crate::shared::error::Result;
use crate::shared::models::Liveness;
use crate::shared::models::timeline::{
    Edge, JobEdge, JobTransition, ReconcileEdge, TimelineQuery, Transition,
};
use crate::shared::util::to_rfc3339_utc;

use super::filters;

/// Job starts and completions in the window, newest first.
///
/// One `job_event` row yields up to TWO edges at different instants, so the two
/// ends are selected separately and unioned rather than derived from one row —
/// a job that started inside the window and has not finished contributes only
/// its start, and one that finished inside a window it started before
/// contributes only its completion. Filtering the union (rather than each half)
/// keeps the org/runner predicate written once.
pub(super) fn job_edges(
    conn: &Connection,
    q: &TimelineQuery,
    limit: usize,
) -> Result<Vec<JobTransition>> {
    let (org, runner) = filters(q);
    let mut stmt = conn.prepare_cached(
        "SELECT ts, org, runner_name, repo, job, started, conclusion FROM ( \
             SELECT started_at AS ts, org, runner_name, repo, job, 1 AS started, \
                    NULL AS conclusion \
             FROM job_event WHERE started_at IS NOT NULL AND started_at >= ?1 \
             UNION ALL \
             SELECT completed_at AS ts, org, runner_name, repo, job, 0 AS started, conclusion \
             FROM job_event WHERE completed_at IS NOT NULL AND completed_at >= ?1 \
         ) WHERE (?2 IS NULL OR org = ?2) AND (?3 IS NULL OR runner_name = ?3) \
         ORDER BY ts DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![q.since_ts, org, runner, limit as i64], |r| {
        let ts: i64 = r.get(0)?;
        Ok(JobTransition {
            ts,
            at: to_rfc3339_utc(ts),
            org: r.get(1)?,
            runner: r.get(2)?,
            repo: r.get(3)?,
            job: r.get(4)?,
            edge: if r.get::<_, i64>(5)? != 0 {
                JobEdge::Started
            } else {
                JobEdge::Completed {
                    conclusion: r.get(6)?,
                }
            },
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Local liveness edges — a runner's process state changing between two
/// consecutive samples.
///
/// The first sample inside the window has no predecessor and so yields no edge:
/// it establishes the baseline. A change that happened exactly at the window's
/// opening tick is therefore attributed to before the window, which is the
/// conservative direction — better to omit an edge we cannot date than to
/// invent one from a value we never observed.
pub(super) fn liveness_edges(
    conn: &Connection,
    q: &TimelineQuery,
    limit: usize,
) -> Result<Vec<Transition>> {
    let (org, runner) = filters(q);
    let mut stmt = conn.prepare_cached(
        "SELECT ts, org, name, prev, liveness FROM ( \
             SELECT r.ts, r.org, r.name, r.liveness, \
                    LAG(r.liveness) OVER (PARTITION BY r.org, r.agent_id ORDER BY r.ts) AS prev \
             FROM runner_sample r \
             WHERE r.ts >= ?1 AND (?2 IS NULL OR r.org = ?2) AND (?3 IS NULL OR r.name = ?3) \
         ) WHERE prev IS NOT NULL AND prev <> liveness \
         ORDER BY ts DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![q.since_ts, org, runner, limit as i64], |r| {
        let ts: i64 = r.get(0)?;
        Ok(Transition {
            ts,
            at: to_rfc3339_utc(ts),
            org: r.get(1)?,
            edge: Edge::Liveness {
                runner: r.get(2)?,
                from: Liveness::from_db(&r.get::<_, String>(3)?),
                to: Liveness::from_db(&r.get::<_, String>(4)?),
            },
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// GitHub-side online edges, from the reconcile samples.
///
/// Derived from consecutive READINGS, not from the local tick grid: an edge
/// means GitHub told us something different from last time it told us anything.
/// A reconcile gap therefore produces no edge — we did not observe a change, we
/// stopped observing, and those are different claims. The gap itself shows up as
/// a `Reconcile` edge if the fetch failed, and as staleness in `--samples`.
pub(super) fn github_edges(
    conn: &Connection,
    q: &TimelineQuery,
    limit: usize,
) -> Result<Vec<Transition>> {
    let (org, runner) = filters(q);
    let mut stmt = conn.prepare_cached(
        "SELECT ts, org, name, online FROM ( \
             SELECT a.ts, a.org, a.name, a.online, \
                    LAG(a.online) OVER (PARTITION BY a.org, a.agent_id ORDER BY a.ts) AS prev \
             FROM api_runner_sample a \
             WHERE a.ts >= ?1 AND (?2 IS NULL OR a.org = ?2) AND (?3 IS NULL OR a.name = ?3) \
         ) WHERE prev IS NOT NULL AND prev <> online \
         ORDER BY ts DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![q.since_ts, org, runner, limit as i64], |r| {
        let ts: i64 = r.get(0)?;
        Ok(Transition {
            ts,
            at: to_rfc3339_utc(ts),
            org: r.get(1)?,
            edge: Edge::GithubOnline {
                runner: r.get(2)?,
                online: r.get::<_, i64>(3)? != 0,
            },
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Per-org reconcile edges — whether we were in a position to hold an opinion
/// about GitHub at all.
///
/// `--runner` does NOT filter these out: they are the context that makes the
/// runner's own edges readable. Narrowing to one runner instead narrows to the
/// org(s) that runner belongs to, so the reply still answers "could we reach
/// GitHub for this runner's org at the time".
pub(super) fn reconcile_edges(
    conn: &Connection,
    q: &TimelineQuery,
    limit: usize,
) -> Result<Vec<Transition>> {
    let (org, runner) = filters(q);
    let mut stmt = conn.prepare_cached(
        "SELECT ts, org, ok, error_kind, http_status FROM ( \
             SELECT s.ts, s.org, s.ok, s.error_kind, s.http_status, \
                    LAG(s.ok) OVER (PARTITION BY s.org ORDER BY s.ts) AS prev \
             FROM api_reconcile_sample s \
             WHERE s.ts >= ?1 AND (?2 IS NULL OR s.org = ?2) \
               AND (?3 IS NULL OR s.org IN \
                    (SELECT DISTINCT org FROM runner_sample WHERE name = ?3)) \
         ) WHERE prev IS NOT NULL AND prev <> ok \
         ORDER BY ts DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![q.since_ts, org, runner, limit as i64], |r| {
        let ts: i64 = r.get(0)?;
        let ok: i64 = r.get(2)?;
        Ok(Transition {
            ts,
            at: to_rfc3339_utc(ts),
            org: r.get(1)?,
            edge: Edge::Reconcile(if ok != 0 {
                ReconcileEdge::Recovered
            } else {
                ReconcileEdge::Failed {
                    error_kind: r.get(3)?,
                    http_status: r.get::<_, Option<i64>>(4)?.map(|v| v as u16),
                }
            }),
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
