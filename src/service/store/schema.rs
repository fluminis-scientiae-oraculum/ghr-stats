use rusqlite::Connection;

use crate::shared::error::Result;

/// Ordered DDL migrations. Append-only: each new entry bumps the schema by one
/// and is tracked via SQLite's `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6];

/// Apply any migrations newer than the DB's recorded `user_version`.
///
/// Refuses a database written by a NEWER build. Migrations are append-only, so
/// this build knows every schema up to its own and nothing about the ones after
/// it: it cannot know which columns a later migration made `NOT NULL`, which
/// table it re-keyed, or what a row it writes would mean to the build that owns
/// the file. Proceeding is a silent write against a contract we do not have —
/// and the way that surfaces is the collector failing an INSERT in its sampling
/// loop hours later, which reads as a runtime bug rather than as the downgrade
/// it is.
///
/// This makes rollback *say so* instead of half-working: to run an older binary,
/// restore a database it wrote. The guard only binds builds that carry it — an
/// already-installed older binary has the permissive code and is unaffected — so
/// it constrains downgrades from here forward, not the rollback path already on
/// disk.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let target = MIGRATIONS.len() as i64;
    if current > target {
        return Err(crate::shared::error::Error::Config(format!(
            "database schema is v{current}, but this build knows only v{target} — the database \
             was written by a NEWER ghr-stats. Upgrade the binary, or point this one at a \
             database it wrote; migrations are append-only, so an older build cannot know what \
             a newer schema requires."
        )));
    }
    if current == target {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for sql in MIGRATIONS.iter().skip(current as usize) {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", target)?;
    tx.commit()?;
    Ok(())
}

const V1: &str = r#"
CREATE TABLE runner_sample (
    ts             INTEGER NOT NULL,
    agent_id       INTEGER NOT NULL,
    name           TEXT    NOT NULL,
    org            TEXT    NOT NULL,
    liveness       TEXT    NOT NULL,
    current_run_id INTEGER,
    cpu_pct        REAL,
    mem_bytes      INTEGER,
    uptime_s       INTEGER
);
CREATE INDEX idx_runner_sample_ts ON runner_sample(ts);
CREATE INDEX idx_runner_sample_agent ON runner_sample(agent_id, ts);

CREATE TABLE host_sample (
    ts         INTEGER NOT NULL,
    load1      REAL    NOT NULL,
    load5      REAL    NOT NULL,
    mem_used   INTEGER NOT NULL,
    mem_total  INTEGER NOT NULL,
    numa_json  TEXT,
    work_bytes INTEGER,
    tmp_bytes  INTEGER,
    root_free  INTEGER
);
CREATE INDEX idx_host_sample_ts ON host_sample(ts);

CREATE TABLE job_event (
    run_id       INTEGER NOT NULL,
    run_attempt  INTEGER NOT NULL DEFAULT 1,
    job          TEXT    NOT NULL DEFAULT '',
    repo         TEXT    NOT NULL DEFAULT '',
    org          TEXT    NOT NULL DEFAULT '',
    runner_name  TEXT    NOT NULL DEFAULT '',
    started_at   INTEGER,
    completed_at INTEGER,
    conclusion   TEXT,
    source       TEXT    NOT NULL DEFAULT 'hook',
    PRIMARY KEY (run_id, run_attempt, job, runner_name)
);
CREATE INDEX idx_job_event_started ON job_event(started_at);
CREATE INDEX idx_job_event_runner ON job_event(runner_name, started_at);

CREATE TABLE queue_sample (
    ts          INTEGER NOT NULL,
    org         TEXT    NOT NULL,
    queued      INTEGER NOT NULL,
    in_progress INTEGER NOT NULL
);
CREATE INDEX idx_queue_sample_ts ON queue_sample(ts);

CREATE TABLE ingest_offset (
    stream TEXT    PRIMARY KEY,
    offset INTEGER NOT NULL
);
"#;

/// v2 — GitHub API reconcile: runner online/busy as GitHub sees it.
const V2: &str = r#"
CREATE TABLE api_runner_sample (
    ts       INTEGER NOT NULL,
    agent_id INTEGER NOT NULL,
    org      TEXT    NOT NULL,
    name     TEXT    NOT NULL,
    online   INTEGER NOT NULL,
    busy     INTEGER NOT NULL
);
CREATE INDEX idx_api_runner_sample_ts ON api_runner_sample(ts);
CREATE INDEX idx_api_runner_sample_agent ON api_runner_sample(agent_id);
"#;

/// v3 — per-runner current liveness with the timestamp of the last *change*
/// (the edge). One row per runner; lets the TUI show "Idle 2h" / "Active 5m"
/// and survives TUI/daemon restarts (the edge isn't kept only in memory).
const V3: &str = r#"
CREATE TABLE runner_state (
    agent_id     INTEGER PRIMARY KEY,
    liveness     TEXT    NOT NULL,
    since_ts     INTEGER NOT NULL,
    last_seen_ts INTEGER NOT NULL
);
"#;

/// v4 — key per-runner identity by install `dir`, not `agentId`. GitHub's
/// `agentId` is unique only *within* an org, so two runners in different orgs
/// can share one; keying local state by it conflated them (cross-contaminated
/// CPU% and a shared liveness edge). Add `dir` to the sample and re-key
/// `runner_state`. Dropping the old `runner_state` rows is safe — they are
/// transient liveness edges that re-populate on the next sampling tick.
const V4: &str = r#"
ALTER TABLE runner_sample ADD COLUMN dir TEXT NOT NULL DEFAULT '';
DROP TABLE runner_state;
CREATE TABLE runner_state (
    dir          TEXT PRIMARY KEY,
    liveness     TEXT    NOT NULL,
    since_ts     INTEGER NOT NULL,
    last_seen_ts INTEGER NOT NULL
);
"#;

/// v5 — `mem_bytes` now holds the working set (anon+shmem); this records the raw
/// cache-inclusive `memory.current` alongside it so the
/// `ghr_runner_mem_current_bytes` gauge keeps cache-vs-working-set pressure
/// observable. Pre-existing rows backfill `NULL` (unknown), which the exporter
/// simply omits.
const V5: &str = r#"
ALTER TABLE runner_sample ADD COLUMN mem_current_bytes INTEGER;
"#;

/// v6 — make the GitHub view *answerable*, not merely recorded.
///
/// `api_runner_sample` already held what GitHub reported at each tick, but three
/// questions an operator asks during an outage had no answer: how long has this
/// runner been offline to GitHub (no edge ⇒ no duration, and the instantaneous
/// bit flaps too much to alert on), did we actually reach GitHub this tick or are
/// we serving a stale read (no reconcile health ⇒ a dead reconcile presents as a
/// calm fleet), and what happened over the last hour (no per-tick outcome).
///
/// `api_runner_state` mirrors `runner_state` on the GitHub side, keyed by
/// `(org, agent_id)` — the API join key. Note the deliberate asymmetry with v4,
/// which re-keyed the LOCAL state to `dir`: `agentId` is unique only *within* an
/// org, so it is a valid key here precisely because `org` is part of it.
/// `since_ts` stays monotonic across a flap, which is what makes a debounced
/// ">15m offline" alert possible at all.
const V6: &str = r#"
CREATE TABLE api_runner_state (
    org          TEXT    NOT NULL,
    agent_id     INTEGER NOT NULL,
    online       INTEGER NOT NULL,
    since_ts     INTEGER NOT NULL,
    last_seen_ts INTEGER NOT NULL,
    PRIMARY KEY (org, agent_id)
);

CREATE TABLE api_reconcile_state (
    org         TEXT PRIMARY KEY,
    last_ok_ts  INTEGER,
    last_try_ts INTEGER NOT NULL,
    ok          INTEGER NOT NULL,
    http_status INTEGER,
    error_kind  TEXT,
    configured  INTEGER NOT NULL
);

CREATE TABLE api_reconcile_sample (
    ts          INTEGER NOT NULL,
    org         TEXT    NOT NULL,
    ok          INTEGER NOT NULL,
    http_status INTEGER,
    error_kind  TEXT,
    runners     INTEGER NOT NULL
);
CREATE INDEX idx_api_reconcile_sample_ts ON api_reconcile_sample(ts);

CREATE INDEX idx_api_runner_sample_org_agent_ts
    ON api_runner_sample(org, agent_id, ts);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A database from the future is refused, not silently written to. The
    /// failure it prevents is not at open time — it is an INSERT failing inside
    /// the collector's sampling loop hours later, against a column a migration
    /// this build has never seen made `NOT NULL`.
    #[test]
    fn a_database_written_by_a_newer_build_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();

        let e = migrate(&mut conn).unwrap_err().to_string();
        assert!(e.contains("written by a NEWER ghr-stats"), "{e}");
        // The message must name both versions: "which binary do I need" is the
        // only question the operator has at that moment.
        assert!(e.contains(&format!("v{}", MIGRATIONS.len() + 1)), "{e}");
        assert!(e.contains(&format!("v{}", MIGRATIONS.len())), "{e}");
    }

    #[test]
    fn migrate_creates_tables_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        // Running again must be a no-op, not an error (idempotency).
        migrate(&mut conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);

        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN \
                 ('runner_sample','host_sample','job_event','queue_sample','ingest_offset',\
                  'api_runner_sample','runner_state','api_runner_state',\
                  'api_reconcile_state','api_reconcile_sample')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 10);
    }
}
