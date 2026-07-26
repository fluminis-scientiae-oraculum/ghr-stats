//! Server-side reads (the IPC server + the metrics exporter). The collector is
//! the only writer; readers open their own connection and rely on WAL for
//! contention-free concurrent reads. The TUI no longer opens the DB — in
//! Persistent mode it fetches these same shapes over the IPC socket, so the
//! query structs below double as the IPC wire payloads (hence the serde derives).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::shared::error::Result;
use crate::shared::models::timeline::{
    Bounded, Edge, ReconcileEdge, Timeline, TimelinePoint, TimelineQuery, Transition, Window,
};
use crate::shared::models::{
    ApiReconcileState, ApiRunnerState, ApiState, BusyPoint, GhCount, GhView, HistPoint, HostPoint,
    JobRow, Liveness, PendingConclusion, RunnerSample, RunnerState,
};
use crate::shared::util::to_rfc3339_utc;

/// The most recent `limit` samples for a runner, returned oldest → newest so
/// they can be fed straight into a left-to-right sparkline.
pub fn runner_history(conn: &Connection, dir: &str, limit: usize) -> Result<Vec<HistPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, cpu_pct, mem_bytes FROM runner_sample \
         WHERE dir = ?1 ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![dir, limit as i64], |r| {
        Ok(HistPoint {
            ts: r.get(0)?,
            cpu_pct: r.get::<_, Option<f64>>(1)?.map(|v| v as f32),
            mem_bytes: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
        })
    })?;
    let mut out: Vec<HistPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

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

/// Recent host samples, oldest → newest.
pub fn host_series(conn: &Connection, limit: usize) -> Result<Vec<HostPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, load1, mem_used, mem_total, tmp_bytes, work_bytes, root_free FROM host_sample \
         ORDER BY ts DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        Ok(HostPoint {
            ts: r.get(0)?,
            load1: r.get(1)?,
            mem_used: r.get::<_, i64>(2)? as u64,
            mem_total: r.get::<_, i64>(3)? as u64,
            tmp_bytes: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
            work_bytes: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            root_free: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        })
    })?;
    let mut out: Vec<HostPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

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

/// Fleet occupancy per tick (busy and online counts), oldest → newest.
///
/// The GitHub count is LEFT-joined per runner from the newest reconcile reading
/// AT OR BEFORE that tick, bounded by `max_age` — the same shape
/// [`timeline_samples`] uses, for the same reason. The two producers are
/// independent threads on different periods (local ticks default to 5s, API
/// ticks to 60s) that each stamp their own clock, so joining on exact `ts`
/// equality only matched when both happened to fire inside the same second:
/// roughly one point per twelve, and the density tracked scheduler drift rather
/// than whether GitHub data existed. Carrying the last reading forward gives a
/// continuous series; `max_age` is what keeps it honest, so a dead reconcile
/// thread decays into a gap instead of a confident flat line.
///
/// Joining per `(org, agent_id)` also narrows the count to OUR runners: the old
/// query summed every API row at the matching tick, which on an org whose
/// runners are spread across hosts counted machines this host cannot see.
///
/// Deriving occupancy from local liveness alone is what let the Trends chart
/// draw a flat healthy line straight through a four-hour outage.
pub fn busy_series(conn: &Connection, limit: usize, max_age: u64) -> Result<Vec<BusyPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT r.ts, \
                SUM(r.liveness = 'busy') AS busy, \
                SUM(r.liveness <> 'offline') AS online, \
                SUM(a.online) AS gh_online, \
                SUM(a.ts IS NOT NULL) AS gh_known \
         FROM runner_sample r \
         LEFT JOIN api_runner_sample a \
                ON a.org = r.org AND a.agent_id = r.agent_id \
               AND a.ts = (SELECT max(x.ts) FROM api_runner_sample x \
                            WHERE x.org = r.org AND x.agent_id = r.agent_id \
                              AND x.ts <= r.ts AND x.ts >= r.ts - ?2) \
         GROUP BY r.ts ORDER BY r.ts DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64, max_age as i64], |r| {
        Ok(BusyPoint {
            ts: r.get(0)?,
            busy: r.get::<_, i64>(1)? as u32,
            online: r.get::<_, i64>(2)? as u32,
            // `gh_online` is NULL exactly when no row joined, which is the same
            // condition as `gh_known == 0`; `GhCount::new` owns that decision.
            github: GhCount::new(
                r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u32,
                r.get::<_, i64>(4)? as u32,
            ),
        })
    })?;
    let mut out: Vec<BusyPoint> = rows.collect::<std::result::Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

/// Every runner's most recent sample (the latest tick). Consumed by the metrics
/// exporter (`metrics::encode`); empty if the collector has never sampled.
pub fn latest_runners(conn: &Connection) -> Result<Vec<RunnerSample>> {
    let max_ts: Option<i64> =
        conn.query_row("SELECT max(ts) FROM runner_sample", [], |r| r.get(0))?;
    let Some(ts) = max_ts else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare_cached(
        "SELECT ts, agent_id, name, org, liveness, current_run_id, cpu_pct, mem_bytes, uptime_s, dir, mem_current_bytes \
         FROM runner_sample WHERE ts = ?1",
    )?;
    let rows = stmt.query_map(params![ts], |r| {
        Ok(RunnerSample {
            ts: r.get(0)?,
            agent_id: r.get(1)?,
            name: r.get(2)?,
            org: r.get(3)?,
            liveness: Liveness::from_db(&r.get::<_, String>(4)?),
            current_run_id: r.get(5)?,
            cpu_pct: r.get::<_, Option<f64>>(6)?.map(|v| v as f32),
            mem_bytes: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
            uptime_s: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
            dir: r.get(9)?,
            mem_current_bytes: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Current liveness + since-edge timestamp per runner, keyed by install `dir`.
/// Drives the "Idle/Active for <dur>" display.
pub fn runner_states(conn: &Connection) -> Result<HashMap<String, RunnerState>> {
    let mut stmt =
        conn.prepare_cached("SELECT dir, liveness, since_ts, last_seen_ts FROM runner_state")?;
    let rows = stmt.query_map([], |r| {
        let dir: String = r.get(0)?;
        Ok((
            dir.clone(),
            RunnerState {
                dir,
                liveness: Liveness::from_db(&r.get::<_, String>(1)?),
                since_ts: r.get(2)?,
                last_seen_ts: r.get(3)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The most recent host sample, if any (for the metrics exporter + banners).
pub fn latest_host(conn: &Connection) -> Result<Option<HostPoint>> {
    Ok(host_series(conn, 1)?.pop())
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

/// Assemble the `timeline` answer for one window: the edges, optionally the raw
/// samples, and how much of the requested window the DB can actually cover.
///
/// The three edge streams are derived **in SQL** (`LAG` over each series'
/// partition) rather than by scanning the window into memory and diffing it.
/// That is what makes the bound real: a six-hour window on this fleet is ~90 000
/// local samples, and a `LIMIT` applied after materialising them would bound the
/// *reply* while leaving the collector to pay for all of it.
pub fn timeline(conn: &Connection, q: &TimelineQuery, now: i64, max_age: u64) -> Result<Timeline> {
    // One row over the limit per stream: enough to know the union was cut,
    // without fetching a second page to find out.
    let probe = q.limit.saturating_add(1);
    let mut rows = liveness_edges(conn, q, probe)?;
    rows.extend(github_edges(conn, q, probe)?);
    rows.extend(reconcile_edges(conn, q, probe)?);
    // Newest first, ready for `Bounded::newest`. `sort_by_key` is stable, so
    // edges sharing a timestamp keep a deterministic order rather than
    // reshuffling between calls over identical data.
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
        samples,
    })
}

/// The optional `--org` / `--runner` filters as SQL parameters.
///
/// Expressed as `(?n IS NULL OR col = ?n)` rather than by concatenating
/// predicates onto the query text: one statement shape, so `prepare_cached`
/// actually caches, and no string building anywhere near a query.
fn filters(q: &TimelineQuery) -> (Option<&str>, Option<&str>) {
    (q.org.as_deref(), q.runner.as_deref())
}

/// Local liveness edges — a runner's process state changing between two
/// consecutive samples.
///
/// The first sample inside the window has no predecessor and so yields no edge:
/// it establishes the baseline. A change that happened exactly at the window's
/// opening tick is therefore attributed to before the window, which is the
/// conservative direction — better to omit an edge we cannot date than to
/// invent one from a value we never observed.
fn liveness_edges(conn: &Connection, q: &TimelineQuery, limit: usize) -> Result<Vec<Transition>> {
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
fn github_edges(conn: &Connection, q: &TimelineQuery, limit: usize) -> Result<Vec<Transition>> {
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
fn reconcile_edges(conn: &Connection, q: &TimelineQuery, limit: usize) -> Result<Vec<Transition>> {
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

/// Raw per-tick rows: local liveness, plus GitHub's view AS OF that tick.
///
/// The GitHub half joins to the newest reconcile reading at or before the tick,
/// never to one taken after it — a timeline must not answer a question with
/// evidence that did not exist yet. Freshness is then adjudicated against the
/// tick through the same [`GhView::observed`] the live path uses, so an hour-old
/// reading reads as stale here exactly as it would there.
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
            liveness: Liveness::from_db(&r.get::<_, String>(3)?),
            github,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The oldest local sample still held under the request's filters — the floor a
/// window can actually reach after `db prune` has run. `None` on an empty DB.
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
    use super::*;

    fn mem_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut conn);
        conn
    }

    #[test]
    fn history_is_chronological_and_limited() {
        let conn = mem_db();
        for ts in [100, 200, 300, 400] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, cpu_pct, mem_bytes, dir) \
                 VALUES (?1, 7, 'r', 'o', 'idle', ?2, ?3, '/srv/r7')",
                params![ts, (ts as f64) / 10.0, ts * 1000],
            )
            .unwrap();
        }
        let h = runner_history(&conn, "/srv/r7", 3).unwrap();
        // newest 3, oldest → newest
        assert_eq!(
            h.iter().map(|p| p.ts).collect::<Vec<_>>(),
            vec![200, 300, 400]
        );
        assert_eq!(h.last().unwrap().mem_bytes, Some(400_000));
        assert!(runner_history(&conn, "/srv/nobody", 10).unwrap().is_empty());
    }

    #[test]
    fn busy_series_counts_busy_and_online_per_tick() {
        let conn = mem_db();
        // tick 100: two idle, one busy, one offline → busy=1 online=3
        for (id, live) in [(1, "idle"), (2, "busy"), (3, "idle"), (4, "offline")] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness) \
                 VALUES (100, ?1, 'r', 'o', ?2)",
                params![id, live],
            )
            .unwrap();
        }
        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].busy, s[0].online), (1, 3));
    }

    fn api_sample(conn: &Connection, ts: i64, org: &str, id: i64, online: i64, busy: i64) {
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (?1, ?2, ?3, 'r', ?4, ?5)",
            params![ts, id, org, online, busy],
        )
        .unwrap();
    }

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

    /// A tick with local samples but no reconcile data must plot a GAP. Emitting
    /// 0 would draw "GitHub says nothing is online" for a fleet nobody asked
    /// GitHub about — inventing an outage instead of admitting ignorance.
    #[test]
    fn busy_series_plots_a_gap_not_a_zero_for_a_tick_without_api_data() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
             VALUES (100, 1, 'r', 'o', 'idle', '/d1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
             VALUES (200, 1, 'r', 'o', 'idle', '/d1')",
            [],
        )
        .unwrap();
        // Only tick 200 has a reconcile row.
        api_sample(&conn, 200, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 2);
        // Tick 100 predates every reading — a gap, and never a zero. A reading
        // is only ever carried FORWARD; inventing GitHub's opinion of a moment
        // before it was asked would be a different lie in the same family.
        assert!(s[0].github.is_none());
        let gh = s[1].github.unwrap();
        assert_eq!((gh.online, gh.known), (1, 1));
    }

    /// The bug this fixes: the two producers are independent threads on
    /// different periods (5s local, 60s API) that each stamp their own clock, so
    /// an exact-`ts` join matched only when both fired inside the same second.
    /// Every tick between two reconciles reported a gap, and the series' density
    /// tracked scheduler drift rather than whether GitHub data existed.
    #[test]
    fn busy_series_carries_the_newest_reading_at_or_before_each_tick() {
        let conn = mem_db();
        for ts in [100, 105, 110, 115] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (?1, 1, 'r', 'o', 'idle', '/d1')",
                params![ts],
            )
            .unwrap();
        }
        // One reconcile, landing on the first tick only.
        api_sample(&conn, 100, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert_eq!(s.len(), 4);
        // All four ticks carry it: under the old exact-`ts` join only the first
        // did, so three of every four points vanished.
        for p in &s {
            let gh = p.github.expect("reading carried forward");
            assert_eq!((gh.online, gh.known), (1, 1));
        }
    }

    /// Carrying forward is only honest while the reading is fresh. Past
    /// `max_age` the series must decay into a gap — otherwise a dead reconcile
    /// thread draws a confident flat line forever, which is the failure the
    /// whole GitHub-view rework exists to prevent.
    #[test]
    fn busy_series_stops_carrying_a_reading_past_max_age() {
        let conn = mem_db();
        for ts in [100, 200, 400] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (?1, 1, 'r', 'o', 'idle', '/d1')",
                params![ts],
            )
            .unwrap();
        }
        api_sample(&conn, 100, "o", 1, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        assert!(s[0].github.is_some()); // age 0
        assert!(s[1].github.is_some()); // age 100, inside the window
        assert!(s[2].github.is_none()); // age 300, past it — a gap
    }

    /// The count speaks only for runners it has a reading for, and says so.
    /// A fleet holding an org GitHub is never asked about (a personal account
    /// has no org runner API) would otherwise draw a permanent divergence: the
    /// GitHub line sitting below the local one forever, for runners nobody ever
    /// asked about. `known` is what tells that silence apart from an outage.
    #[test]
    fn busy_series_counts_only_the_runners_it_has_a_reading_for() {
        let conn = mem_db();
        for (id, org) in [(1, "asked"), (2, "never-asked")] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (100, ?1, 'r', ?2, 'idle', ?3)",
                params![id, org, format!("/d{id}")],
            )
            .unwrap();
        }
        api_sample(&conn, 100, "asked", 1, 1, 0);
        // A runner GitHub knows about that this host does not run — another
        // machine in the same org. It must not inflate our count.
        api_sample(&conn, 100, "asked", 99, 1, 0);

        let s = busy_series(&conn, 10, 180).unwrap();
        let gh = s[0].github.unwrap();
        assert_eq!(s[0].online, 2);
        assert_eq!((gh.online, gh.known), (1, 1));
    }

    #[test]
    fn host_series_chronological() {
        let conn = mem_db();
        for ts in [10, 20, 30] {
            conn.execute(
                "INSERT INTO host_sample (ts, load1, load5, mem_used, mem_total, tmp_bytes) \
                 VALUES (?1, 1.0, 1.0, 100, 200, ?2)",
                params![ts, ts * 5],
            )
            .unwrap();
        }
        let s = host_series(&conn, 2).unwrap();
        assert_eq!(s.iter().map(|p| p.ts).collect::<Vec<_>>(), vec![20, 30]);
        assert_eq!(s.last().unwrap().tmp_bytes, Some(150));
        assert_eq!(s[0].work_bytes, None);
    }

    #[test]
    fn latest_runners_uses_newest_tick() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO runner_sample (ts,agent_id,name,org,liveness) VALUES (100,1,'r1','o','idle')",
            [],
        )
        .unwrap();
        for (id, name, live) in [(1, "r1", "busy"), (2, "r2", "idle")] {
            conn.execute(
                "INSERT INTO runner_sample (ts,agent_id,name,org,liveness) VALUES (200,?1,?2,'o',?3)",
                params![id, name, live],
            )
            .unwrap();
        }
        let rows = latest_runners(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.ts == 200));
        let r1 = rows.iter().find(|r| r.agent_id == 1).unwrap();
        assert_eq!(r1.liveness, Liveness::Busy);
        assert!(latest_runners(&mem_db()).unwrap().is_empty());
    }

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

    /// The point of the verb: 100 samples that say "still idle" collapse to the
    /// two moments the answer actually changed.
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
        // Chronological, and both ends of each edge are carried.
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

    /// The first sample inside a window has no predecessor, so it is a baseline,
    /// not an edge. Emitting one would date a change to a moment we never saw it
    /// happen — an invented timestamp is worse than a missing one.
    #[test]
    fn the_first_sample_in_a_window_is_a_baseline_not_an_edge() {
        let conn = mem_db();
        local_sample(&conn, 100, "o", 1, "r1", "idle");
        local_sample(&conn, 200, "o", 1, "r1", "busy");
        // Window opens at 200: only the busy row is visible, so nothing to
        // compare against and no edge — even though a change did occur.
        let t = timeline(&conn, &tq(200, 100), 300, 180).unwrap();
        assert!(t.transitions.items.is_empty());
        // Widen the window to include the predecessor and the edge appears.
        let t = timeline(&conn, &tq(100, 100), 300, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 1);
    }

    /// GitHub edges come from consecutive READINGS. A reconcile gap means we
    /// stopped observing, not that nothing changed, so it yields no edge — and
    /// the runner's next reading is compared against its own last one, not
    /// against a tick that happened to be nearby.
    #[test]
    fn github_edges_come_from_consecutive_readings_not_the_local_tick_grid() {
        let conn = mem_db();
        api_named(&conn, 100, "o", 1, "r1", 1);
        // No reading at 200 or 300 — the reconcile was down.
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

    /// The edge that makes the other two readable: eight runners going
    /// GitHub-offline means something different depending on whether we could
    /// still ask GitHub at the time.
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

    /// Narrowing to one runner must NOT drop its org's reconcile edges: they are
    /// the context that says whether the runner's GitHub edges mean anything.
    /// Another org's reconcile noise is still excluded.
    #[test]
    fn a_runner_filter_keeps_its_own_orgs_reconcile_edges() {
        let conn = mem_db();
        local_sample(&conn, 100, "org-a", 1, "r1", "idle");
        local_sample(&conn, 200, "org-a", 1, "r1", "busy");
        reconcile_sample(&conn, 100, "org-a", 1, None);
        reconcile_sample(&conn, 200, "org-a", 0, Some("server_error"));
        // A different org failing at the same moment is not this runner's story.
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

    /// A sample's GitHub column is the newest reading AT OR BEFORE that tick —
    /// never one taken afterwards. A timeline that answered a question with
    /// evidence that did not exist yet would be worse than one that said nothing.
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

        // 800: no reading had been taken at all — unknown, not "offline".
        assert!(matches!(s.items[0].github, GhView::Unknown));
        // 1000: the 900 reading, 100s old, inside the 180s window.
        assert!(matches!(
            s.items[1].github,
            GhView::Fresh { age_s: 100, .. }
        ));
        // 1200: still the 900 reading — the 1300 one had not happened — and at
        // 300s it is past the window, so it reads STALE rather than live.
        assert!(matches!(s.items[2].github, GhView::Stale { age_s: 300 }));
    }

    /// A pruned window and a quiet window both return few rows; only one of them
    /// means "nothing happened", so the floor is reported explicitly.
    #[test]
    fn a_window_reaching_past_retention_reports_where_the_data_starts() {
        let conn = mem_db();
        local_sample(&conn, 5_000, "o", 1, "r1", "idle");
        local_sample(&conn, 5_100, "o", 1, "r1", "busy");

        // Asking from 0 reaches past what is held.
        let t = timeline(&conn, &tq(0, 100), 6_000, 180).unwrap();
        assert_eq!(t.window.truncated_at, Some(5_000));
        // Asking from inside the held range is fully covered.
        let t = timeline(&conn, &tq(5_000, 100), 6_000, 180).unwrap();
        assert_eq!(t.window.truncated_at, None);
    }

    /// The limit applies to the UNION of the three edge streams, keeps the
    /// newest, and says so — a full page that looks complete is the failure this
    /// flag exists to prevent.
    #[test]
    fn the_limit_bounds_the_union_and_reports_the_cut() {
        let conn = mem_db();
        // Alternating liveness gives one edge per sample after the first.
        for (i, ts) in (100..=600).step_by(100).enumerate() {
            let live = if i.is_multiple_of(2) { "idle" } else { "busy" };
            local_sample(&conn, ts, "o", 1, "r1", live);
        }
        // ...and an interleaved reconcile edge, so the cut spans two streams.
        reconcile_sample(&conn, 100, "o", 1, None);
        reconcile_sample(&conn, 550, "o", 0, Some("timeout"));

        let t = timeline(&conn, &tq(0, 3), 700, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 3);
        assert!(t.transitions.limited);
        // The NEWEST three, in chronological order.
        assert_eq!(
            t.transitions.items.iter().map(|x| x.ts).collect::<Vec<_>>(),
            vec![500, 550, 600]
        );

        // Room to spare ⇒ not marked limited.
        let t = timeline(&conn, &tq(0, 100), 700, 180).unwrap();
        assert_eq!(t.transitions.items.len(), 6);
        assert!(!t.transitions.limited);
    }

    /// Samples are omitted unless asked for, and `None` is distinguishable from
    /// an empty set — "you did not ask" and "there is nothing" are different
    /// answers.
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

        // Requested, but the window predates every sample: empty, not absent.
        q.since_ts = 1_000;
        let s = timeline(&conn, &q, 2_000, 180).unwrap().samples.unwrap();
        assert!(s.items.is_empty());
    }

    /// Two runners in different orgs may share an `agent_id` (this fleet has a
    /// real collision). Partitioning by id alone would splice their two series
    /// into one and manufacture edges from the interleaving.
    #[test]
    fn edges_partition_by_org_and_id_not_id_alone() {
        let conn = mem_db();
        // Same agent_id 22, two orgs, each perfectly stable.
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
