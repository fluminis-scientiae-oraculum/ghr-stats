use rusqlite::Connection;

use crate::shared::error::Result;

/// Append-only: entry N is schema vN, recorded in `PRAGMA user_version`.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6];

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

/// `since_ts` is the last liveness change.
const V3: &str = r#"
CREATE TABLE runner_state (
    agent_id     INTEGER PRIMARY KEY,
    liveness     TEXT    NOT NULL,
    since_ts     INTEGER NOT NULL,
    last_seen_ts INTEGER NOT NULL
);
"#;

/// Re-keys local runner state by install `dir`: GitHub's `agentId` is unique only within an org.
/// Dropping `runner_state` is safe; its edges re-populate on the next tick.
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

/// `mem_bytes` holds the working set (anon+shmem); `mem_current_bytes` is the cache-inclusive
/// `memory.current`, NULL on older rows.
const V5: &str = r#"
ALTER TABLE runner_sample ADD COLUMN mem_current_bytes INTEGER;
"#;

/// GitHub-side `runner_state`, keyed `(org, agent_id)` since agentId is unique only within an org.
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

    #[test]
    fn a_database_written_by_a_newer_build_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64 + 1)
            .unwrap();

        let e = migrate(&mut conn).unwrap_err().to_string();
        assert!(e.contains("written by a NEWER ghr-stats"), "{e}");
        assert!(e.contains(&format!("v{}", MIGRATIONS.len() + 1)), "{e}");
        assert!(e.contains(&format!("v{}", MIGRATIONS.len())), "{e}");
    }

    #[test]
    fn migrate_creates_tables_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
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
