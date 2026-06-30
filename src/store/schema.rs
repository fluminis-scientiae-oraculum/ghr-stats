use rusqlite::Connection;

use crate::error::Result;

/// Ordered DDL migrations. Append-only: each new entry bumps the schema by one
/// and is tracked via SQLite's `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[V1, V2];

/// Apply any migrations newer than the DB's recorded `user_version`.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let target = MIGRATIONS.len() as i64;
    if current >= target {
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

#[cfg(test)]
mod tests {
    use super::*;

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
                  'api_runner_sample')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 6);
    }
}
